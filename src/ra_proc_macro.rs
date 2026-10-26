use anyhow::{anyhow, Context as _};
use cargo_metadata as cm;
use itertools::chain;
use maplit::btreemap;
use ra_ap_paths::AbsPath;
use ra_ap_proc_macro_api::{MacroDylib, ProcMacro, ProcMacroClient, ProcMacroKind};
use ra_ap_span::{
    Edition, EditionedFileId, FileId, Span, SpanAnchor, SyntaxContext, ROOT_ERASED_FILE_AST_ID,
};
use ra_ap_tt::{
    self as tt,
    iter::{TtElement, TtIter},
    DelimiterKind, Leaf, TextSize,
};
use semver::Version;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(crate) const MSRV: Version = Version::new(1, 64, 0);

pub(crate) fn list_proc_macro_dylibs<P: FnMut(&cm::PackageId) -> bool>(
    cargo_messages: &[cm::Message],
    mut filter: P,
) -> BTreeMap<&cm::PackageId, &AbsPath> {
    cargo_messages
        .iter()
        .flat_map(|message| match message {
            cm::Message::CompilerArtifact(artifact) => Some(artifact),
            _ => None,
        })
        .filter(|cm::Artifact { target, .. }| *target.kind == ["proc-macro".to_owned()])
        .filter(|cm::Artifact { package_id, .. }| filter(package_id))
        .flat_map(
            |cm::Artifact {
                 package_id,
                 filenames,
                 ..
             }| {
                filenames
                    .first()
                    .map(|filename| (package_id, AbsPath::assert(filename.as_ref())))
            },
        )
        .collect()
}

pub struct ProcMacroExpander<'msg> {
    custom_derive: BTreeMap<String, (&'msg cm::PackageId, ProcMacro)>,
    func_like: BTreeMap<String, (&'msg cm::PackageId, ProcMacro)>,
    attr: BTreeMap<String, (&'msg cm::PackageId, ProcMacro)>,
}

impl<'msg> ProcMacroExpander<'msg> {
    pub(crate) fn spawn(
        proc_macro_srv_exe: &AbsPath,
        dylib_paths: &BTreeMap<&'msg cm::PackageId, &'msg AbsPath>,
    ) -> anyhow::Result<Self> {
        let env: HashMap<String, Option<String>> = HashMap::new();
        let server = ProcMacroClient::spawn(proc_macro_srv_exe, &env)?;

        let mut custom_derive = btreemap!();
        let mut func_like = btreemap!();
        let mut attr = btreemap!();

        for (&package_id, dylib_path) in dylib_paths {
            let proc_macros = server
                .load_dylib(MacroDylib::new(dylib_path.to_path_buf()))
                .map_err(|e| anyhow!("{}", e))
                .with_context(|| "rust-analyzer error")?;

            for proc_macro in proc_macros {
                match proc_macro.kind() {
                    ProcMacroKind::CustomDerive => &mut custom_derive,
                    ProcMacroKind::Bang => &mut func_like,
                    ProcMacroKind::Attr => &mut attr,
                }
                .insert(proc_macro.name().to_owned(), (package_id, proc_macro));
            }
        }

        Ok(Self {
            custom_derive,
            func_like,
            attr,
        })
    }

    pub(crate) fn macro_names(
        &self,
    ) -> impl Iterator<Item = (&'msg cm::PackageId, BTreeSet<&str>)> {
        let mut names = BTreeMap::<_, BTreeSet<_>>::new();
        for (name, &(pkg, _)) in chain!(&self.custom_derive, &self.func_like, &self.attr) {
            names.entry(pkg).or_default().insert(&**name);
        }
        names.into_iter()
    }

    pub(crate) fn attempt_expand_custom_derive(
        &mut self,
        name: &str,
        body: impl FnOnce() -> proc_macro2::TokenStream,
    ) -> anyhow::Result<Option<proc_macro2::Group>> {
        self.attempt_expand(name, ProcMacroKind::CustomDerive, body, None::<fn() -> _>)
    }

    pub(crate) fn attempt_expand_func_like(
        &mut self,
        name: &str,
        body: impl FnOnce() -> proc_macro2::TokenStream,
    ) -> anyhow::Result<Option<proc_macro2::Group>> {
        self.attempt_expand(name, ProcMacroKind::Bang, body, None::<fn() -> _>)
    }

    pub(crate) fn attempt_expand_attr(
        &mut self,
        name: &str,
        body: impl FnOnce() -> proc_macro2::TokenStream,
        attr: impl FnOnce() -> proc_macro2::Group,
    ) -> anyhow::Result<Option<proc_macro2::Group>> {
        self.attempt_expand(name, ProcMacroKind::Attr, body, Some(attr))
    }

    fn attempt_expand(
        &self,
        name: &str,
        kind: ProcMacroKind,
        subtree: impl FnOnce() -> proc_macro2::TokenStream,
        attr: Option<impl FnOnce() -> proc_macro2::Group>,
    ) -> anyhow::Result<Option<proc_macro2::Group>> {
        match kind {
            ProcMacroKind::CustomDerive => &self.custom_derive,
            ProcMacroKind::Bang => &self.func_like,
            ProcMacroKind::Attr => &self.attr,
        }
        .get(name)
        .map(|(_, proc_macro)| {
            let subtree = from_proc_macro2_group(&proc_macro2::Group::new(
                proc_macro2::Delimiter::None,
                subtree(),
            ));
            let attr = attr.map(|f| from_proc_macro2_group(&f()));
            let span = dummy_span();
            let output = proc_macro
                .expand(
                    subtree.view(),
                    attr.as_ref().map(|attr| attr.view()),
                    vec![],
                    span,
                    span,
                    span,
                    String::new(),
                )
                .map_err(|e| anyhow!("{}", e))
                .with_context(|| "rust-analyzer error")?
                .map_err(|s| anyhow!("proc macro panicked: {s:?}"))?;
            Ok(from_ra_top_subtree(output))
        })
        .transpose()
    }
}

fn dummy_span() -> Span {
    Span {
        range: tt::TextRange::empty(TextSize::new(0)),
        anchor: SpanAnchor {
            file_id: EditionedFileId::current_edition(FileId::from_raw(0)),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

fn from_proc_macro2_group(group: &proc_macro2::Group) -> tt::TopSubtree<Span> {
    let span = dummy_span();
    let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter {
        open: span,
        close: span,
        kind: from_proc_macro2_delimiter(group.delimiter()),
    });
    for token_tree in group.stream() {
        push_proc_macro2_token_tree(&mut builder, &token_tree);
    }
    builder.build()
}

fn push_proc_macro2_token_tree(
    builder: &mut tt::TopSubtreeBuilder<Span>,
    token_tree: &proc_macro2::TokenTree,
) {
    let span = dummy_span();
    match token_tree {
        proc_macro2::TokenTree::Group(group) => {
            builder.open(from_proc_macro2_delimiter(group.delimiter()), span);
            for token_tree in group.stream() {
                push_proc_macro2_token_tree(builder, &token_tree);
            }
            builder.close(span);
        }
        proc_macro2::TokenTree::Ident(ident) => {
            builder.push(Leaf::Ident(tt::Ident::new(&ident.to_string(), span)));
        }
        proc_macro2::TokenTree::Punct(punct) => {
            builder.push(Leaf::Punct(tt::Punct {
                char: punct.as_char(),
                spacing: from_proc_macro2_spacing(punct.spacing()),
                span,
            }));
        }
        proc_macro2::TokenTree::Literal(literal) => {
            builder.push(Leaf::Literal(tt::token_to_literal(
                &literal.to_string(),
                span,
            )));
        }
    }
}

fn from_proc_macro2_delimiter(delimiter: proc_macro2::Delimiter) -> DelimiterKind {
    match delimiter {
        proc_macro2::Delimiter::Parenthesis => DelimiterKind::Parenthesis,
        proc_macro2::Delimiter::Brace => DelimiterKind::Brace,
        proc_macro2::Delimiter::Bracket => DelimiterKind::Bracket,
        proc_macro2::Delimiter::None => DelimiterKind::Invisible,
    }
}

fn from_proc_macro2_spacing(spacing: proc_macro2::Spacing) -> tt::Spacing {
    match spacing {
        proc_macro2::Spacing::Alone => tt::Spacing::Alone,
        proc_macro2::Spacing::Joint => tt::Spacing::Joint,
    }
}

fn from_ra_top_subtree(subtree: tt::TopSubtree<Span>) -> proc_macro2::Group {
    let view = subtree.view();
    from_ra_subtree(view.top_subtree(), view.iter())
}

fn from_ra_subtree(subtree: &tt::Subtree<Span>, iter: TtIter<'_, Span>) -> proc_macro2::Group {
    proc_macro2::Group::new(
        from_ra_delimiter(subtree.delimiter.kind),
        iter.map(from_ra_token_tree).collect(),
    )
}

fn from_ra_token_tree(token_tree: TtElement<'_, Span>) -> proc_macro2::TokenTree {
    match token_tree {
        TtElement::Leaf(Leaf::Ident(ident)) => from_ra_ident(ident).into(),
        TtElement::Leaf(Leaf::Punct(punct)) => from_ra_punct(*punct).into(),
        TtElement::Leaf(Leaf::Literal(literal)) => from_ra_literal(literal).into(),
        TtElement::Subtree(subtree, iter) => from_ra_subtree(subtree, iter).into(),
    }
}

fn from_ra_delimiter(delimiter: DelimiterKind) -> proc_macro2::Delimiter {
    match delimiter {
        DelimiterKind::Parenthesis => proc_macro2::Delimiter::Parenthesis,
        DelimiterKind::Brace => proc_macro2::Delimiter::Brace,
        DelimiterKind::Bracket => proc_macro2::Delimiter::Bracket,
        DelimiterKind::Invisible => proc_macro2::Delimiter::None,
    }
}

fn from_ra_ident(ident: &tt::Ident<Span>) -> proc_macro2::Ident {
    let span = proc_macro2::Span::call_site();
    if ident.is_raw.yes() {
        proc_macro2::Ident::new_raw(ident.sym.as_str(), span)
    } else {
        proc_macro2::Ident::new(ident.sym.as_str(), span)
    }
}

fn from_ra_punct(punct: tt::Punct<Span>) -> proc_macro2::Punct {
    proc_macro2::Punct::new(punct.char, from_ra_spacing(punct.spacing))
}

fn from_ra_spacing(spacing: tt::Spacing) -> proc_macro2::Spacing {
    match spacing {
        tt::Spacing::Alone | tt::Spacing::JointHidden => proc_macro2::Spacing::Alone,
        tt::Spacing::Joint => proc_macro2::Spacing::Joint,
    }
}

fn from_ra_literal(literal: &tt::Literal<Span>) -> proc_macro2::Literal {
    let text = literal.to_string();
    syn::parse_str(&text)
        .unwrap_or_else(|e| panic!("could not parse {:?} as a literal: {}", text, e))
}
