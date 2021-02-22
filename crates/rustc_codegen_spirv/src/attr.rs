//! `#[spirv(...)]` attribute support.
//!
//! The attribute-checking parts of this try to follow `rustc_passes::check_attr`.

use crate::codegen_cx::CodegenCx;
use crate::symbols::Symbols;
use rspirv::spirv::{
    AccessQualifier, BuiltIn, Dim, ExecutionMode, ExecutionModel, ImageFormat, StorageClass,
};
use rustc_ast::Attribute;
use rustc_hir as hir;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::{self, NestedVisitorMap, Visitor};
use rustc_hir::{HirId, MethodKind, Target, CRATE_HIR_ID};
use rustc_middle::hir::map::Map;
use rustc_middle::ty::query::Providers;
use rustc_middle::ty::TyCtxt;
use rustc_span::{Span, Symbol};
use std::rc::Rc;

// FIXME(eddyb) replace with `ArrayVec<[Word; 3]>`.
#[derive(Copy, Clone, Debug)]
pub struct ExecutionModeExtra {
    args: [u32; 3],
    len: u8,
}

impl ExecutionModeExtra {
    pub(crate) fn new(args: impl AsRef<[u32]>) -> Self {
        let _args = args.as_ref();
        let mut args = [0; 3];
        args[.._args.len()].copy_from_slice(_args);
        let len = _args.len() as u8;
        Self { args, len }
    }
}

impl AsRef<[u32]> for ExecutionModeExtra {
    fn as_ref(&self) -> &[u32] {
        &self.args[..self.len as _]
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub execution_model: ExecutionModel,
    pub execution_modes: Vec<(ExecutionMode, ExecutionModeExtra)>,
    pub name: Option<Symbol>,
}

impl From<ExecutionModel> for Entry {
    fn from(execution_model: ExecutionModel) -> Self {
        Self {
            execution_model,
            execution_modes: Vec::new(),
            name: None,
        }
    }
}

/// `struct` types that are used to represent special SPIR-V types.
#[derive(Debug, Clone)]
pub enum IntrinsicType {
    ImageType {
        dim: Dim,
        depth: u32,
        arrayed: u32,
        multisampled: u32,
        sampled: u32,
        image_format: ImageFormat,
        access_qualifier: Option<AccessQualifier>,
    },
    Sampler,
    SampledImage,
}

// NOTE(eddyb) when adding new `#[spirv(...)]` attributes, the tests found inside
// `tests/ui/spirv-attr` should be updated (and new ones added if necessary).
#[derive(Debug, Clone)]
pub enum SpirvAttribute {
    // `struct` attributes:
    IntrinsicType(IntrinsicType),
    Block,

    // `fn` attributes:
    Entry(Entry),

    // (entry) `fn` parameter attributes:
    StorageClass(StorageClass),
    Builtin(BuiltIn),
    DescriptorSet(u32),
    Binding(u32),
    Flat,

    // `fn`/closure attributes:
    UnrollLoops,
}

// HACK(eddyb) this is similar to `rustc_span::Spanned` but with `value` as the
// field name instead of `node` (which feels inadequate in this context).
#[derive(Copy, Clone)]
pub struct Spanned<T> {
    pub value: T,
    pub span: Span,
}

/// Condensed version of a `SpirvAttribute` list, but only keeping one value per
/// variant of `SpirvAttribute`, and treating multiple such attributes an error.
// FIXME(eddyb) should this and `fn try_insert_attr` below be generated by a macro?
#[derive(Default)]
pub struct AggregatedSpirvAttributes {
    // `struct` attributes:
    pub intrinsic_type: Option<Spanned<IntrinsicType>>,
    pub block: Option<Spanned<()>>,

    // `fn` attributes:
    pub entry: Option<Spanned<Entry>>,

    // (entry) `fn` parameter attributes:
    pub storage_class: Option<Spanned<StorageClass>>,
    pub builtin: Option<Spanned<BuiltIn>>,
    pub descriptor_set: Option<Spanned<u32>>,
    pub binding: Option<Spanned<u32>>,
    pub flat: Option<Spanned<()>>,

    // `fn`/closure attributes:
    pub unroll_loops: Option<Spanned<()>>,
}

struct MultipleAttrs {
    prev_span: Span,
    category: &'static str,
}

impl AggregatedSpirvAttributes {
    /// Compute `AggregatedSpirvAttributes` for use during codegen.
    ///
    /// Any errors for malformed/duplicate attributes will have been reported
    /// prior to codegen, by the `attr` check pass.
    pub fn parse<'tcx>(cx: &CodegenCx<'tcx>, attrs: &'tcx [Attribute]) -> Self {
        let mut aggregated_attrs = Self::default();

        // NOTE(eddyb) `delay_span_bug` ensures that if attribute checking fails
        // to see an attribute error, it will cause an ICE instead.
        for (_, parse_attr_result) in crate::symbols::parse_attrs_for_checking(&cx.sym, attrs) {
            let (span, parsed_attr) = match parse_attr_result {
                Ok(span_and_parsed_attr) => span_and_parsed_attr,
                Err((span, msg)) => {
                    cx.tcx.sess.delay_span_bug(span, &msg);
                    continue;
                }
            };
            match aggregated_attrs.try_insert_attr(parsed_attr, span) {
                Ok(()) => {}
                Err(MultipleAttrs {
                    prev_span: _,
                    category,
                }) => {
                    cx.tcx
                        .sess
                        .delay_span_bug(span, &format!("multiple {} attributes", category));
                }
            }
        }

        aggregated_attrs
    }

    fn try_insert_attr(&mut self, attr: SpirvAttribute, span: Span) -> Result<(), MultipleAttrs> {
        fn try_insert<T>(
            slot: &mut Option<Spanned<T>>,
            value: T,
            span: Span,
            category: &'static str,
        ) -> Result<(), MultipleAttrs> {
            match slot {
                Some(prev) => Err(MultipleAttrs {
                    prev_span: prev.span,
                    category,
                }),
                None => {
                    *slot = Some(Spanned { value, span });
                    Ok(())
                }
            }
        }

        use SpirvAttribute::*;
        match attr {
            IntrinsicType(value) => {
                try_insert(&mut self.intrinsic_type, value, span, "intrinsic type")
            }
            Block => try_insert(&mut self.block, (), span, "#[spirv(block)]"),
            Entry(value) => try_insert(&mut self.entry, value, span, "entry-point"),
            StorageClass(value) => {
                try_insert(&mut self.storage_class, value, span, "storage class")
            }
            Builtin(value) => try_insert(&mut self.builtin, value, span, "builtin"),
            DescriptorSet(value) => try_insert(
                &mut self.descriptor_set,
                value,
                span,
                "#[spirv(descriptor_set)]",
            ),
            Binding(value) => try_insert(&mut self.binding, value, span, "#[spirv(binding)]"),
            Flat => try_insert(&mut self.flat, (), span, "#[spirv(flat)]"),
            UnrollLoops => try_insert(&mut self.unroll_loops, (), span, "#[spirv(unroll_loops)]"),
        }
    }
}

// FIXME(eddyb) make this reusable from somewhere in `rustc`.
fn target_from_impl_item<'tcx>(tcx: TyCtxt<'tcx>, impl_item: &hir::ImplItem<'_>) -> Target {
    match impl_item.kind {
        hir::ImplItemKind::Const(..) => Target::AssocConst,
        hir::ImplItemKind::Fn(..) => {
            let parent_hir_id = tcx.hir().get_parent_item(impl_item.hir_id());
            let containing_item = tcx.hir().expect_item(parent_hir_id);
            let containing_impl_is_for_trait = match &containing_item.kind {
                hir::ItemKind::Impl(hir::Impl { of_trait, .. }) => of_trait.is_some(),
                _ => unreachable!("parent of an ImplItem must be an Impl"),
            };
            if containing_impl_is_for_trait {
                Target::Method(MethodKind::Trait { body: true })
            } else {
                Target::Method(MethodKind::Inherent)
            }
        }
        hir::ImplItemKind::TyAlias(..) => Target::AssocTy,
    }
}

struct CheckSpirvAttrVisitor<'tcx> {
    tcx: TyCtxt<'tcx>,
    sym: Rc<Symbols>,
}

impl CheckSpirvAttrVisitor<'_> {
    fn check_spirv_attributes(&self, hir_id: HirId, target: Target) {
        let mut aggregated_attrs = AggregatedSpirvAttributes::default();

        let parse_attrs = |attrs| crate::symbols::parse_attrs_for_checking(&self.sym, attrs);

        let attrs = self.tcx.hir().attrs(hir_id);
        for (attr, parse_attr_result) in parse_attrs(attrs) {
            // Make sure to mark the whole `#[spirv(...)]` attribute as used,
            // to avoid warnings about unused attributes.
            self.tcx.sess.mark_attr_used(attr);

            let (span, parsed_attr) = match parse_attr_result {
                Ok(span_and_parsed_attr) => span_and_parsed_attr,
                Err((span, msg)) => {
                    self.tcx.sess.span_err(span, &msg);
                    continue;
                }
            };

            /// Error newtype marker used below for readability.
            struct Expected<T>(T);

            let valid_target = match parsed_attr {
                SpirvAttribute::IntrinsicType(_) | SpirvAttribute::Block => match target {
                    Target::Struct => {
                        // FIXME(eddyb) further check type attribute validity,
                        // e.g. layout, generics, other attributes, etc.
                        Ok(())
                    }

                    _ => Err(Expected("struct")),
                },

                SpirvAttribute::Entry(_) => match target {
                    Target::Fn
                    | Target::Method(MethodKind::Trait { body: true })
                    | Target::Method(MethodKind::Inherent) => {
                        // FIXME(eddyb) further check entry-point attribute validity,
                        // e.g. signature, shouldn't have `#[inline]` or generics, etc.
                        Ok(())
                    }

                    _ => Err(Expected("function")),
                },

                SpirvAttribute::StorageClass(_)
                | SpirvAttribute::Builtin(_)
                | SpirvAttribute::DescriptorSet(_)
                | SpirvAttribute::Binding(_)
                | SpirvAttribute::Flat => match target {
                    Target::Param => {
                        let parent_hir_id = self.tcx.hir().get_parent_node(hir_id);
                        let parent_is_entry_point =
                            parse_attrs(self.tcx.hir().attrs(parent_hir_id))
                                .filter_map(|(_, r)| r.ok())
                                .any(|(_, attr)| matches!(attr, SpirvAttribute::Entry(_)));
                        if !parent_is_entry_point {
                            self.tcx.sess.span_err(
                                span,
                                "attribute is only valid on a parameter of an entry-point function",
                            );
                        }
                        Ok(())
                    }

                    _ => Err(Expected("function parameter")),
                },

                SpirvAttribute::UnrollLoops => match target {
                    Target::Fn
                    | Target::Closure
                    | Target::Method(MethodKind::Trait { body: true })
                    | Target::Method(MethodKind::Inherent) => Ok(()),

                    _ => Err(Expected("function or closure")),
                },
            };
            match valid_target {
                Err(Expected(expected_target)) => self.tcx.sess.span_err(
                    span,
                    &format!(
                        "attribute is only valid on a {}, not on a {}",
                        expected_target, target
                    ),
                ),
                Ok(()) => match aggregated_attrs.try_insert_attr(parsed_attr, span) {
                    Ok(()) => {}
                    Err(MultipleAttrs {
                        prev_span,
                        category,
                    }) => self
                        .tcx
                        .sess
                        .struct_span_err(
                            span,
                            &format!("only one {} attribute is allowed on a {}", category, target),
                        )
                        .span_note(prev_span, &format!("previous {} attribute", category))
                        .emit(),
                },
            }
        }
    }
}

// FIXME(eddyb) DRY this somehow and make it reusable from somewhere in `rustc`.
impl<'tcx> Visitor<'tcx> for CheckSpirvAttrVisitor<'tcx> {
    type Map = Map<'tcx>;

    fn nested_visit_map(&mut self) -> NestedVisitorMap<Self::Map> {
        NestedVisitorMap::OnlyBodies(self.tcx.hir())
    }

    fn visit_item(&mut self, item: &'tcx hir::Item<'tcx>) {
        let target = Target::from_item(item);
        self.check_spirv_attributes(item.hir_id(), target);
        intravisit::walk_item(self, item)
    }

    fn visit_generic_param(&mut self, generic_param: &'tcx hir::GenericParam<'tcx>) {
        let target = Target::from_generic_param(generic_param);
        self.check_spirv_attributes(generic_param.hir_id, target);
        intravisit::walk_generic_param(self, generic_param)
    }

    fn visit_trait_item(&mut self, trait_item: &'tcx hir::TraitItem<'tcx>) {
        let target = Target::from_trait_item(trait_item);
        self.check_spirv_attributes(trait_item.hir_id(), target);
        intravisit::walk_trait_item(self, trait_item)
    }

    fn visit_field_def(&mut self, field: &'tcx hir::FieldDef<'tcx>) {
        self.check_spirv_attributes(field.hir_id, Target::Field);
        intravisit::walk_field_def(self, field);
    }

    fn visit_arm(&mut self, arm: &'tcx hir::Arm<'tcx>) {
        self.check_spirv_attributes(arm.hir_id, Target::Arm);
        intravisit::walk_arm(self, arm);
    }

    fn visit_foreign_item(&mut self, f_item: &'tcx hir::ForeignItem<'tcx>) {
        let target = Target::from_foreign_item(f_item);
        self.check_spirv_attributes(f_item.hir_id(), target);
        intravisit::walk_foreign_item(self, f_item)
    }

    fn visit_impl_item(&mut self, impl_item: &'tcx hir::ImplItem<'tcx>) {
        let target = target_from_impl_item(self.tcx, impl_item);
        self.check_spirv_attributes(impl_item.hir_id(), target);
        intravisit::walk_impl_item(self, impl_item)
    }

    fn visit_stmt(&mut self, stmt: &'tcx hir::Stmt<'tcx>) {
        // When checking statements ignore expressions, they will be checked later.
        if let hir::StmtKind::Local(l) = stmt.kind {
            self.check_spirv_attributes(l.hir_id, Target::Statement);
        }
        intravisit::walk_stmt(self, stmt)
    }

    fn visit_expr(&mut self, expr: &'tcx hir::Expr<'tcx>) {
        let target = match expr.kind {
            hir::ExprKind::Closure(..) => Target::Closure,
            _ => Target::Expression,
        };

        self.check_spirv_attributes(expr.hir_id, target);
        intravisit::walk_expr(self, expr)
    }

    fn visit_variant(
        &mut self,
        variant: &'tcx hir::Variant<'tcx>,
        generics: &'tcx hir::Generics<'tcx>,
        item_id: HirId,
    ) {
        self.check_spirv_attributes(variant.id, Target::Variant);
        intravisit::walk_variant(self, variant, generics, item_id)
    }

    fn visit_macro_def(&mut self, macro_def: &'tcx hir::MacroDef<'tcx>) {
        self.check_spirv_attributes(macro_def.hir_id(), Target::MacroDef);
        intravisit::walk_macro_def(self, macro_def);
    }

    fn visit_param(&mut self, param: &'tcx hir::Param<'tcx>) {
        self.check_spirv_attributes(param.hir_id, Target::Param);

        intravisit::walk_param(self, param);
    }
}

fn check_invalid_macro_level_spirv_attr(tcx: TyCtxt<'_>, sym: &Symbols, attrs: &[Attribute]) {
    for attr in attrs {
        if tcx.sess.check_name(attr, sym.spirv) {
            tcx.sess
                .span_err(attr.span, "#[spirv(..)] cannot be applied to a macro");
        }
    }
}

// FIXME(eddyb) DRY this somehow and make it reusable from somewhere in `rustc`.
fn check_mod_attrs(tcx: TyCtxt<'_>, module_def_id: LocalDefId) {
    let check_spirv_attr_visitor = &mut CheckSpirvAttrVisitor {
        tcx,
        sym: Symbols::get(),
    };
    tcx.hir().visit_item_likes_in_module(
        module_def_id,
        &mut check_spirv_attr_visitor.as_deep_visitor(),
    );
    // FIXME(eddyb) use `tcx.hir().visit_exported_macros_in_krate(...)` after rustup.
    for id in tcx.hir().krate().exported_macros {
        check_spirv_attr_visitor.visit_macro_def(match tcx.hir().find(id.hir_id()) {
            Some(hir::Node::MacroDef(macro_def)) => macro_def,
            _ => unreachable!(),
        });
    }
    check_invalid_macro_level_spirv_attr(
        tcx,
        &check_spirv_attr_visitor.sym,
        tcx.hir().krate().non_exported_macro_attrs,
    );
    if module_def_id.is_top_level_module() {
        check_spirv_attr_visitor.check_spirv_attributes(CRATE_HIR_ID, Target::Mod);
    }
}

pub(crate) fn provide(providers: &mut Providers) {
    *providers = Providers {
        check_mod_attrs: |tcx, def_id| {
            // Run both the default checks, and our `#[spirv(...)]` ones.
            (rustc_interface::DEFAULT_QUERY_PROVIDERS.check_mod_attrs)(tcx, def_id);
            check_mod_attrs(tcx, def_id)
        },
        ..*providers
    };
}