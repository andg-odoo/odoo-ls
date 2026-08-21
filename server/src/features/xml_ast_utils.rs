use crate::{
    core::{
        evaluation::EvaluationSymbolPtr,
        evaluation_context::ContextKey,
        model::Model,
        odoo::SyncOdoo,
        symbols::{
            ModuleSymbol,
            storage::xml::xml_field_symbol::XmlFieldName,
            symbol_keys::{ModuleKey, SourceFileKey, SymbolKey, XmlId},
        },
    },
    features::{features_utils::{FeaturesUtils, SegmentPick}, owl_expr::decode_entities},
    threads::SessionInfo,
};
use roxmltree::{Attribute, Node};
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{Expr, ExprStringLiteral};
use ruff_python_parser::parse_parenthesized_expression_range;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};
use std::{ops::Range, rc::Rc};

/// Tags making a `<field>`'s content an inline subview, `tree` being pre-18.0 for `list`.
const SUBVIEW_TAGS: [&str; 6] = ["form", "list", "tree", "graph", "kanban", "calendar"];

/// Names odoo predefines in an expression, `IGNORED_IN_EXPRESSION` of `tools/view_validation.py`.
const IGNORED_IN_EXPRESSION: [&str; 22] = [
    "True", "False", "None", "self", "uid", "context", "context_today", "allowed_company_ids",
    "current_company_id", "time", "datetime", "relativedelta", "current_date", "today", "now",
    "abs", "len", "bool", "float", "str", "unicode", "set",
];

/// Model a subtree resolves its fields against.
#[derive(Clone, Default)]
pub enum ModelScope {
    #[default]
    None,
    /// Shared rather than owned, as every `<field>` clones the scope to record its own name.
    Known(Rc<str>),
    /// Inside a subview whose comodel did not resolve, which must not fall back to the parent.
    Unknown,
}

impl ModelScope {
    pub fn known(&self) -> Option<&str> {
        match self {
            ModelScope::Known(model) if !model.is_empty() => Some(model),
            _ => None,
        }
    }
}

/// Inherited state threaded top-down through the XML walk, handed to children as a clone.
#[derive(Clone, Default)]
pub struct XmlScope<'a> {
    /// Model the surrounding `<record>`/arch subtree resolves fields against.
    pub record_model: ModelScope,
    /// Model of the view a subview is nested in, what `parent.<field>` resolves against.
    pub parent_model: ModelScope,
    /// `name` of the enclosing `<field>` (drives `<field name="model">` text).
    pub field_name: Option<&'a str>,
    /// For an `ir.ui.view` record, the model its arch targets (captured from the
    /// record's `<field name="model">`). Applied to the `<field name="arch">` subtree.
    pub view_target_model: Option<&'a str>,
}

/// A field name a python attribute holds, and the model it belongs to.
pub enum ExprField {
    /// A dotted path of the model the attribute targets.
    OnTarget(String, Range<usize>),
    /// A name of the model in scope, `parent.<field>` naming one of the model above.
    OnCurrent(String, Range<usize>, bool),
}

/// What an XML location refers to, kept next to the symbols so consumers need not infer it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XmlRefKind {
    /// A model name: `<record model=…>`, `<field name="model">…</field>`.
    Model,
    /// A reference to an xml id: `ref=`, `groups=`, `action=`, `inherit_id=`, `%(…)d`.
    XmlId,
    /// The `id=` of a `<record>`, declaring the xml id rather than referencing one.
    XmlIdDeclaration,
    /// A field or a method of the surrounding record model: `<field name=…>`, `<button name=…>`.
    Member,
}

/// A resolvable XML location: its kind, byte range, and symbols, empty when nothing matched.
pub struct XmlRef {
    pub kind: XmlRefKind,
    pub range: Range<usize>,
    pub symbols: Vec<SymbolKey>,
}

pub struct XmlAstUtils {}

impl XmlAstUtils {

    pub fn get_symbols(session: &mut SessionInfo, file_symbol: SourceFileKey, root: roxmltree::Node, offset: usize, on_dep_only: bool) -> (Vec<SymbolKey>, Option<Range<usize>>) {
        let mut results = (vec![], None);
        XmlAstUtils::visit_document(session, file_symbol, root, Some(offset), on_dep_only, &mut |xml_ref| {
            results.0.extend(xml_ref.symbols);
            results.1 = Some(xml_ref.range);
        });
        results
    }

    /// Every resolvable location of the document in source order, the walk without a cursor.
    pub fn collect_refs(session: &mut SessionInfo, file_symbol: SourceFileKey, root: roxmltree::Node, on_dep_only: bool) -> Vec<XmlRef> {
        let mut refs = vec![];
        XmlAstUtils::visit_document(session, file_symbol, root, None, on_dep_only, &mut |xml_ref| refs.push(xml_ref));
        refs
    }

    fn visit_document(session: &mut SessionInfo, file_symbol: SourceFileKey, root: roxmltree::Node, offset: Option<usize>, on_dep_only: bool, out: &mut dyn FnMut(XmlRef)) {
        let from_module = session.sync_odoo.symbol_table.find_module(file_symbol);
        for node in root.children() {
            XmlAstUtils::visit_node(session, &node, offset, from_module, &XmlScope::default(), out, on_dep_only);
        }
    }

    /// Whether `range` is under the cursor, `None` taking every range.
    fn is_at_offset(range: &Range<usize>, offset: Option<usize>) -> bool {
        offset.is_none_or(|offset| range.start <= offset && offset <= range.end)
    }

    fn visit_node<'a>(session: &mut SessionInfo<'_>, node: &Node<'a, '_>, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope<'a>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        if offset.is_some_and(|offset| node.range().start > offset) {
            return;
        }
        if node.is_element() {
            XmlAstUtils::scan_format_xml_id_refs(session, node, offset, from_module, out, on_dep_only);
            // `t-call` names a qweb view by xml id, a frontend template name resolving to nothing.
            XmlAstUtils::emit_attribute_xml_ids(session, node, offset, from_module, &["t-call"], out, on_dep_only);
            // Any element may carry `groups`, a list rather than the single id the others hold.
            XmlAstUtils::emit_group_ids(session, node, offset, from_module, out, on_dep_only);
            // Any element may carry a python expression over the fields in scope.
            XmlAstUtils::emit_expression_fields(session, node, offset, from_module, scope, out, on_dep_only);
            match node.tag_name().name()  {
                "record" => {
                    XmlAstUtils::visit_record(session, node, offset, from_module, scope, out, on_dep_only);
                }
                // `<groupby name="X">` names a many2one field and switches to its comodel.
                "field" | "groupby" => {
                    XmlAstUtils::visit_field(session, node, offset, from_module, scope, out, on_dep_only);
                },
                "menuitem" => {
                    XmlAstUtils::visit_menu_item(session, node, offset, from_module, scope, out, on_dep_only);
                },
                "template" => {
                    XmlAstUtils::visit_template(session, node, offset, from_module, scope, out, on_dep_only);
                }
                "button" => {
                    XmlAstUtils::visit_button(session, node, offset, from_module, scope, out, on_dep_only);
                }
                _ => {
                    for child in node.children() {
                        XmlAstUtils::visit_node(session, &child, offset, from_module, scope, out, on_dep_only);
                    }
                }
            }
        } else if node.is_text() {
            XmlAstUtils::visit_text(session, node, offset, from_module, scope, out, on_dep_only);
        }
    }

    fn visit_button<'a>(session: &mut SessionInfo<'_>, node: &Node<'a, '_>, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope<'a>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        // An implicit `type` is `object` in views, only `type="action"` names an xml id.
        let is_action_type = node.attribute("type") == Some("action");
        for attr in node.attributes() {
            if !XmlAstUtils::is_at_offset(&attr.range_value(), offset) {
                continue;
            }
            if attr.name() == "name" && !is_action_type
                && let Some(model_name) = scope.record_model.known()
            {
                let found = XmlAstUtils::resolve_member_on_model(session, model_name, attr.value(), from_module, on_dep_only);
                if !found.is_empty() {
                    out(XmlRef { kind: XmlRefKind::Member, range: attr.range_value(), symbols: found });
                }
            }
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module, scope, out, on_dep_only);
        }
    }

    /// Find a member named `name` on every class registered for `model_name`,
    /// honoring `_inherit` chains. Returns the concrete Variable/Function
    /// symbols. Drives goto-def / hover for `<field name="X"/>` and
    /// `<button name="X"/>` resolution.
    fn resolve_member_on_model(session: &mut SessionInfo, model_name: &str, member_name: &str, from_module: Option<ModuleKey>, on_dep_only: bool) -> Vec<SymbolKey> {
        let mut out = Vec::new();
        let Some(model) = session.sync_odoo.models.get(model_name).cloned() else { return out };
        let from_module = if on_dep_only { from_module } else { None };
        for class_key in Model::get_full_model_classes(model.clone(), session, from_module) {
            let content = session.st().get_content_symbol(class_key.into(), member_name, u32::MAX);
            out.extend(content.symbols);
        }
        let model_ref = model.borrow();
        for xml_record_key in model_ref.get_xml_model_field_symbols(session.st(), from_module) {
            let field_name = session.st()[xml_record_key].get_field_text(XmlFieldName::Name, session.st());
            if field_name.as_deref() == Some(member_name) {
                out.push(xml_record_key.into());
            }
        }
        out
    }

    fn visit_record<'a>(session: &mut SessionInfo<'_>, node: &Node<'a, '_>, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope<'a>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        let mut scope = scope.clone();
        for attr in node.attributes() {
            if attr.name() == "model" {
                scope.record_model = ModelScope::Known(Rc::from(attr.value()));
                if XmlAstUtils::is_at_offset(&attr.range_value(), offset)
                    && let Some(model) = session.sync_odoo.models.get(attr.value()).cloned()
                {
                    let from_module = match on_dep_only {
                        true => from_module,
                        false => None,
                    };
                    let symbols = model.borrow().get_model_symbols(session.st(), from_module).map(SymbolKey::from).collect();
                    out(XmlRef { kind: XmlRefKind::Model, range: attr.range_value(), symbols });
                }
            } else if attr.name() == "id"
                && XmlAstUtils::is_at_offset(&attr.range_value(), offset)
                && let Some(file_module) = from_module
            {
                XmlAstUtils::emit_xml_id(session, XmlRefKind::XmlIdDeclaration, attr.value(), file_module, attr.range_value(), out, on_dep_only);
            }
        }
        if scope.record_model.known() == Some("ir.ui.view") {
            scope.view_target_model = XmlAstUtils::view_target_model(node);
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module, &scope, out, on_dep_only);
        }
    }

    fn visit_field<'a>(session: &mut SessionInfo<'_>, node: &Node<'a, '_>, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope<'a>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        let mut field_name = None;
        for attr in node.attributes() {
            if attr.name() == "name" {
                field_name = Some(attr.value());
                if XmlAstUtils::is_at_offset(&attr.range_value(), offset)
                    && let Some(model_name) = scope.record_model.known()
                {
                    let found = XmlAstUtils::resolve_member_on_model(session, model_name, attr.value(), from_module, on_dep_only);
                    if !found.is_empty() {
                        out(XmlRef { kind: XmlRefKind::Member, range: attr.range_value(), symbols: found });
                    }
                }
            } else if attr.name() == "ref"
                && XmlAstUtils::is_at_offset(&attr.range_value(), offset)
                && let Some(file_module) = from_module
            {
                XmlAstUtils::emit_xml_id(session, XmlRefKind::XmlId, attr.value(), file_module, attr.range_value(), out, on_dep_only);
            }
        }
        // A childless `<field name="x"/>` is the common case and has no subtree to scope.
        if !node.has_children() {
            return;
        }
        let mut child_scope = scope.clone();
        child_scope.field_name = field_name;
        if let Some(model_scope) = XmlAstUtils::child_model_scope(session, node, scope, from_module, on_dep_only) {
            // Only a subview nests a model in another, the arch of a view has none above it.
            if field_name != Some("arch") {
                child_scope.parent_model = child_scope.record_model.clone();
            }
            child_scope.record_model = model_scope;
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module, &child_scope, out, on_dep_only);
        }
    }

    /// Scope in effect for `node`, rebuilt by descending the ancestors from the document root.
    pub fn scope_at<'a>(session: &mut SessionInfo, node: &Node<'a, '_>, from_module: Option<ModuleKey>, on_dep_only: bool) -> XmlScope<'a> {
        let mut ancestors = node.ancestors().filter(|ancestor| ancestor.is_element()).collect::<Vec<_>>();
        ancestors.reverse();
        // The attributes of `node` itself are resolved in the scope of its parent.
        ancestors.pop();
        let mut scope = XmlScope::default();
        for ancestor in ancestors {
            match ancestor.tag_name().name() {
                "record" => {
                    if let Some(model) = ancestor.attribute("model") {
                        scope.record_model = ModelScope::Known(Rc::from(model));
                    }
                    if scope.record_model.known() == Some("ir.ui.view") {
                        scope.view_target_model = XmlAstUtils::view_target_model(&ancestor);
                    }
                },
                "field" | "groupby" => {
                    scope.field_name = ancestor.attribute("name");
                    if let Some(model_scope) = XmlAstUtils::child_model_scope(session, &ancestor, &scope, from_module, on_dep_only) {
                        if scope.field_name != Some("arch") {
                            scope.parent_model = scope.record_model.clone();
                        }
                        scope.record_model = model_scope;
                    }
                },
                _ => {},
            }
        }
        scope
    }

    /// Model the children of a `<field>`/`<groupby>` resolve against, when it differs.
    pub fn child_model_scope<'a>(session: &mut SessionInfo, node: &Node<'a, '_>, scope: &XmlScope<'a>, from_module: Option<ModuleKey>, on_dep_only: bool) -> Option<ModelScope> {
        let model_name = scope.record_model.known()?;
        let field_name = node.attribute("name")?;
        if node.tag_name().name() == "field" {
            // Inside a view's `arch`, sub-elements resolve against the view's target model.
            if field_name == "arch" {
                return scope.view_target_model.map(|target| ModelScope::Known(Rc::from(target)));
            }
            if !node.children().any(|child| child.is_element() && SUBVIEW_TAGS.contains(&child.tag_name().name())) {
                return None;
            }
        }
        Some(match XmlAstUtils::comodel_name(session, model_name, field_name, from_module, on_dep_only) {
            Some(comodel) => ModelScope::Known(comodel),
            None => ModelScope::Unknown,
        })
    }

    /// Comodel of `field_name` on `model_name`, read from the field without following refs.
    pub fn comodel_name(session: &mut SessionInfo, model_name: &str, field_name: &str, from_module: Option<ModuleKey>, on_dep_only: bool) -> Option<Rc<str>> {
        for field in XmlAstUtils::resolve_member_on_model(session, model_name, field_name, from_module, on_dep_only) {
            match field {
                SymbolKey::Variable(variable_key) => {
                    for eval in session.st()[variable_key].evaluations.clone().iter() {
                        if let EvaluationSymbolPtr::WEAK(weak) = eval.symbol.get_symbol(session, None, &mut vec![], None)
                            && let Some(comodel) = weak.context.get(ContextKey::ComodelName)
                        {
                            return Some(Rc::from(comodel.as_str()));
                        }
                    }
                },
                SymbolKey::XmlRecord(record_key) => {
                    let ttype = session.st()[record_key].get_field_text(XmlFieldName::Type, session.st());
                    if !ttype.is_some_and(|ttype| ["many2one", "many2many", "one2many"].contains(&ttype.as_str())) {
                        continue;
                    }
                    if let Some(relation) = session.st()[record_key].get_field_text(XmlFieldName::Relation, session.st()) {
                        return Some(Rc::from(relation.as_str()));
                    }
                },
                _ => {},
            }
        }
        None
    }

    fn visit_text(session: &mut SessionInfo, node: &Node, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        if XmlAstUtils::is_at_offset(&node.range(), offset) {
            let (Some(_model), Some(field)) = (
                scope.record_model.known(),
                scope.field_name.filter(|f| !f.is_empty()),
            ) else {
                return;
            };
            if field == "model" || field == "res_model" { //do not check model, let's assume it will contains a model name
                XmlAstUtils::emit_model(session, node, from_module, out, on_dep_only);
            }
        }
    }

    fn visit_menu_item<'a>(session: &mut SessionInfo<'_>, node: &Node<'a, '_>, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope<'a>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        XmlAstUtils::emit_attribute_xml_ids(session, node, offset, from_module, &["action", "parent"], out, on_dep_only);
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module, scope, out, on_dep_only);
        }
    }

    fn visit_template<'a>(session: &mut SessionInfo<'_>, node: &Node<'a, '_>, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope<'a>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        XmlAstUtils::emit_attribute_xml_ids(session, node, offset, from_module, &["inherit_id"], out, on_dep_only);
        if let Some(id) = node.attribute_node("id")
            && XmlAstUtils::is_at_offset(&id.range_value(), offset)
            && let Some(file_module) = from_module
        {
            XmlAstUtils::emit_xml_id(session, XmlRefKind::XmlIdDeclaration, id.value(), file_module, id.range_value(), out, on_dep_only);
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module, scope, out, on_dep_only);
        }
    }

    /// Resolve each of `attr_names` present on `node` as a plain xml-id reference.
    fn emit_attribute_xml_ids(session: &mut SessionInfo, node: &Node, offset: Option<usize>, from_module: Option<ModuleKey>, attr_names: &[&str], out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        let Some(file_module) = from_module else { return };
        for attr in node.attributes() {
            if XmlAstUtils::is_at_offset(&attr.range_value(), offset) && attr_names.contains(&attr.name()) {
                XmlAstUtils::emit_xml_id(session, XmlRefKind::XmlId, attr.value(), file_module, attr.range_value(), out, on_dep_only);
            }
        }
    }

    /// Resolve each group named by a `groups` attribute on `node` as its own xml-id reference.
    fn emit_group_ids(session: &mut SessionInfo, node: &Node, offset: Option<usize>, from_module: Option<ModuleKey>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        let Some(file_module) = from_module else { return };
        let Some(attr) = node.attributes().find(|attr| attr.name() == "groups") else { return };
        if !XmlAstUtils::is_at_offset(&attr.range_value(), offset) {
            return;
        }
        let mut hits: Vec<(String, Range<usize>)> = vec![];
        XmlAstUtils::for_each_group_id(&attr, node.document().input_text(), |group, range| {
            if XmlAstUtils::is_at_offset(&range, offset) {
                hits.push((group.to_string(), range));
            }
        });
        for (group, range) in hits {
            XmlAstUtils::emit_xml_id(session, XmlRefKind::XmlId, &group, file_module, range, out, on_dep_only);
        }
    }

    /// Resolve the fields named by every python expression carried by `node`.
    fn emit_expression_fields(session: &mut SessionInfo, node: &Node, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        let doc_text = node.document().input_text();
        let mut hits: Vec<ExprField> = vec![];
        for attr in node.attributes() {
            if !XmlAstUtils::is_expression_attribute(attr.name()) || !XmlAstUtils::is_at_offset(&attr.range_value(), offset) {
                continue;
            }
            XmlAstUtils::for_each_expression_field(&attr, doc_text, |field| {
                let (ExprField::OnTarget(_, range) | ExprField::OnCurrent(_, range, _)) = &field;
                if XmlAstUtils::is_at_offset(range, offset) {
                    hits.push(field);
                }
            });
        }
        // Only a path needs the target model, and resolving it costs a comodel lookup.
        let target_model = match hits.iter().any(|hit| matches!(hit, ExprField::OnTarget(..))) {
            true => XmlAstUtils::expression_target_model(session, node, scope, from_module, on_dep_only),
            false => None,
        };
        for hit in hits {
            match hit {
                ExprField::OnCurrent(name, range, on_parent) => {
                    let model = match on_parent {
                        true => &scope.parent_model,
                        false => &scope.record_model,
                    };
                    let Some(model_name) = model.known() else { continue };
                    let found = XmlAstUtils::resolve_member_on_model(session, model_name, &name, from_module, on_dep_only);
                    if !found.is_empty() {
                        out(XmlRef { kind: XmlRefKind::Member, range, symbols: found });
                    }
                },
                ExprField::OnTarget(path, range) => {
                    let Some(target_model) = target_model.clone() else { continue };
                    XmlAstUtils::emit_path_fields(session, &target_model, &path, range, offset, from_module, out, on_dep_only);
                },
            }
        }
    }

    /// Resolve `path`, a dotted field path of `model_name`, as one reference per segment.
    fn emit_path_fields(session: &mut SessionInfo, model_name: &str, path: &str, range: Range<usize>, offset: Option<usize>, from_module: Option<ModuleKey>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        let pick = match offset {
            Some(offset) => SegmentPick::Cursor(offset),
            None => SegmentPick::All,
        };
        let from_module = match on_dep_only {
            true => from_module,
            false => None,
        };
        // `find_model_fields` starts its segments one byte past the range, where the quote sits.
        let path_range = TextRange::new(TextSize::new(range.start as u32 - 1), TextSize::new(range.end as u32));
        let mut segments: Vec<(Range<usize>, Vec<SymbolKey>)> = vec![];
        for (symbol, segment) in FeaturesUtils::find_model_fields(session, model_name, from_module, &path_range, path, pick) {
            let segment = usize::from(segment.start())..usize::from(segment.end());
            match segments.last_mut() {
                Some((last, symbols)) if *last == segment => symbols.push(symbol),
                _ => segments.push((segment, vec![symbol])),
            }
        }
        for (range, symbols) in segments {
            out(XmlRef { kind: XmlRefKind::Member, range, symbols });
        }
    }

    /// Model a `domain`/`context` on `node` names fields of, the comodel of the field it is set on.
    pub fn expression_target_model(session: &mut SessionInfo, node: &Node, scope: &XmlScope, from_module: Option<ModuleKey>, on_dep_only: bool) -> Option<Rc<str>> {
        let model_name = scope.record_model.known()?;
        // Only a `<field>` retargets, at the comodel it filters, a `<filter>` staying on the view.
        if node.tag_name().name() != "field" {
            return Some(Rc::from(model_name));
        }
        XmlAstUtils::comodel_name(session, model_name, node.attribute("name")?, from_module, on_dep_only)
    }

    fn scan_format_xml_id_refs(session: &mut SessionInfo, node: &Node, offset: Option<usize>, from_module: Option<ModuleKey>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        let Some(file_module) = from_module else { return };
        let doc_text = node.document().input_text();
        let mut hits: Vec<(String, Range<usize>)> = vec![];
        for attr in node.attributes() {
            if !XmlAstUtils::is_at_offset(&attr.range_value(), offset) { continue; }
            XmlAstUtils::for_each_format_xml_id_ref(&attr, doc_text, |inner, range| {
                if XmlAstUtils::is_at_offset(&range, offset) {
                    hits.push((inner.to_string(), range));
                }
            });
        }
        for (inner, range) in hits {
            XmlAstUtils::emit_xml_id(session, XmlRefKind::XmlId, &inner, file_module, range, out, on_dep_only);
        }
    }

    /// For each `%(xml_id)d|s|i` format-string reference in `attr`'s value, invoke
    /// `f` with the inner xml-id and its absolute byte range (excluding the
    /// `%(` and `)X` wrapper). Used for `<button name="%(...)d">` style action refs.
    ///
    /// Scans `doc_text` (the raw source) rather than the entity-decoded
    /// `attr.value()`, so offsets stay aligned with `range_value()` even when
    /// the attribute contains an entity like `&amp;`.
    pub fn for_each_format_xml_id_ref(attr: &Attribute, doc_text: &str, mut f: impl FnMut(&str, Range<usize>)) {
        let attr_range = attr.range_value();
        let attr_start = attr_range.start;
        let value = &doc_text[attr_range];
        let bytes = value.as_bytes();
        let mut i = 0;
        while i + 3 < bytes.len() {
            if bytes[i] == b'%' && bytes[i + 1] == b'('
                && let Some(close_off) = bytes[i + 2..].iter().position(|&b| b == b')')
            {
                let inner_start = i + 2;
                let inner_end = i + 2 + close_off;
                let after = inner_end + 1;
                if after < bytes.len() && matches!(bytes[after], b'd' | b's' | b'i') {
                    f(&value[inner_start..inner_end], attr_start + inner_start..attr_start + inner_end);
                    i = after + 1;
                    continue;
                }
            }
            i += 1;
        }
    }

    /// Each comma-separated group of `attr` with its range, `!` negated and `.` NO_ACCESS skipped.
    pub fn for_each_group_id(attr: &Attribute, doc_text: &str, mut f: impl FnMut(&str, Range<usize>)) {
        let attr_range = attr.range_value();
        let attr_start = attr_range.start;
        let value = &doc_text[attr_range];
        let mut segment_start = 0;
        for segment in value.split(',') {
            let group = segment.trim().trim_start_matches('!').trim_start();
            if !group.is_empty() && group != "." && let Some(offset) = segment.find(group) {
                let start = attr_start + segment_start + offset;
                f(group, start..start + group.len());
            }
            segment_start += segment.len() + 1;
        }
    }

    /// Whether `name` holds a python expression over the fields in scope, a modifier or a filter.
    pub fn is_expression_attribute(name: &str) -> bool {
        matches!(name, "invisible" | "column_invisible" | "readonly" | "required" | "domain" | "context")
            || name.starts_with("decoration-")
    }

    /// Each field name of the python expression of `attr`, with the model it belongs to.
    pub fn for_each_expression_field(attr: &Attribute, doc_text: &str, mut f: impl FnMut(ExprField)) {
        let attr_range = attr.range_value();
        let attr_start = attr_range.start;
        // The decode pads what it shortens, so parsing raw `doc_text` keeps offsets file-aligned.
        let expr = decode_entities(&doc_text[attr_range]);
        // A value spread over several lines only parses as if parenthesized, as lxml joins it.
        let Ok(parsed) = parse_parenthesized_expression_range(&expr, TextRange::up_to(expr.text_len())) else { return };
        let shifted = |range: TextRange| attr_start + usize::from(range.start())..attr_start + usize::from(range.end());
        if attr.name() == "domain" {
            for_each_domain_leaf(parsed.expr(), &mut |field| {
                f(ExprField::OnTarget(field.value.to_str().to_string(), shifted(literal_content(field))));
            });
        }
        let mut visitor = ExpressionValueVisitor { hits: vec![] };
        visitor.visit_expr(parsed.expr());
        for (name, range, on_parent) in visitor.hits {
            f(ExprField::OnCurrent(name, shifted(range), on_parent));
        }
    }

    /// For an `ir.ui.view` record, the model its arch targets, read from the
    /// record's direct `<field name="model">…</field>` child.
    pub fn view_target_model<'a>(record: &Node<'a, '_>) -> Option<&'a str> {
        for child in record.children() {
            if child.is_element()
                && child.tag_name().name() == "field"
                && child.attribute("name") == Some("model")
                && let Some(text) = child.text()
            {
                let model = text.trim();
                if !model.is_empty() {
                    return Some(model);
                }
            }
        }
        None
    }

    fn emit_model(session: &mut SessionInfo, node: &Node, from_module: Option<ModuleKey>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        if let Some(model) = session.sync_odoo.models.get(node.text().unwrap()).cloned() {
            let from_module = match on_dep_only {
                true => from_module,
                false => None,
            };
            let symbols = model.borrow().get_model_symbols(session.st(), from_module).map(SymbolKey::from).collect();
            out(XmlRef { kind: XmlRefKind::Model, range: node.range(), symbols });
        }
    }

    fn emit_xml_id(session: &mut SessionInfo, kind: XmlRefKind, xml_id: &str, file_module: ModuleKey, range: Range<usize>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        let file_symbol: SourceFileKey = file_module.into();
        let xml_ids = SyncOdoo::get_xml_ids(session, file_symbol, xml_id, &range, &mut vec![]);

        let mut symbols = vec![];
        for xml_id in xml_ids.iter_valid(session.st()) {
            if on_dep_only
                && let Some(module) = session.st().find_module(xml_id)
                    && !ModuleSymbol::is_in_deps(
                        session.st(),
                        session.st().find_module(file_symbol).unwrap(),
                        &session.st()[module].name,
                    ) {
                        continue;
                    }
            match xml_id {
                XmlId::PythonClass(key) => symbols.push(key.into()),
                XmlId::XmlRecord(key) => symbols.push(key.into()),
                XmlId::XmlMenuItem(key) => symbols.push(key.into()),
                XmlId::XmlTemplate(key) => symbols.push(key.into()),
                XmlId::XmlAsset(key) => symbols.push(key.into()),
                // `<delete id=…>` names the records to remove, it declares nothing.
                XmlId::XmlDelete(_) => {},
            }
        }
        out(XmlRef { kind, range, symbols });
    }

    /**
     * Clear invalid weak values from js_templates for this template name.
     * Return true if there is still valid values after the cleanup
     */
    pub fn ensure_js_template_validity(session: &mut SessionInfo, t_name: &str) -> bool {
        let Some(templates) = session.sync_odoo.js_templates.get(t_name) else {
            return false;
        };
        if templates.is_empty(&session.sync_odoo.symbol_table) {
            session.sync_odoo.js_templates.remove(t_name);
            return false;
        }
        true
    }

}

/// Byte range of a string literal's content, the quotes around it excluded.
fn literal_content(literal: &ExprStringLiteral) -> TextRange {
    TextRange::new(literal.start() + TextSize::new(1), literal.end() - TextSize::new(1))
}

/// Left-hand side of each leaf of `domain`, only a direct item of the domain list being one.
fn for_each_domain_leaf(domain: &Expr, f: &mut impl FnMut(&ExprStringLiteral)) {
    let items = match domain {
        Expr::List(list) => &list.elts,
        Expr::Tuple(tuple) => &tuple.elts,
        _ => return,
    };
    for item in items {
        let elements = match item {
            Expr::List(list) => &list.elts,
            Expr::Tuple(tuple) => &tuple.elts,
            _ => continue,
        };
        if let [Expr::StringLiteral(field), _, _] = &elements[..] {
            f(field);
        }
    }
}

/// Collects the names an expression uses as values, `parent.<field>` belonging to the model above.
struct ExpressionValueVisitor {
    hits: Vec<(String, TextRange, bool)>,
}

impl Visitor<'_> for ExpressionValueVisitor {
    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Attribute(attribute) if matches!(attribute.value.as_ref(), Expr::Name(name) if name.id.as_str() == "parent") => {
                self.hits.push((attribute.attr.to_string(), attribute.attr.range(), true));
            },
            Expr::Name(name) if !IGNORED_IN_EXPRESSION.contains(&name.id.as_str()) => {
                self.hits.push((name.id.to_string(), name.range(), false));
            },
            // Any other attribute has only its value walked, `context.get` naming no field.
            _ => walk_expr(self, expr),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Value names and absolute ranges of the first attribute of `xml`'s root element.
    fn modifier_fields(xml: &str) -> Vec<(String, Range<usize>, bool)> {
        expression_fields(xml).into_iter().filter_map(|field| match field {
            ExprField::OnCurrent(name, range, on_parent) => Some((name, range, on_parent)),
            ExprField::OnTarget(..) => None,
        }).collect()
    }

    /// Every field named by the first attribute of `xml`'s root element, ranges checked.
    fn expression_fields(xml: &str) -> Vec<ExprField> {
        let doc = roxmltree::Document::parse(xml).unwrap();
        let attr = doc.root_element().attributes().next().unwrap();
        let mut hits = vec![];
        XmlAstUtils::for_each_expression_field(&attr, xml, |field| {
            let (ExprField::OnTarget(name, range) | ExprField::OnCurrent(name, range, _)) = &field;
            assert_eq!(&xml[range.clone()], name, "range does not cover the name it reports");
            hits.push(field);
        });
        hits
    }

    /// Field paths of the target model named by the first attribute of `xml`'s root element.
    fn target_fields(xml: &str) -> Vec<(String, Range<usize>)> {
        expression_fields(xml).into_iter().filter_map(|field| match field {
            ExprField::OnTarget(name, range) => Some((name, range)),
            ExprField::OnCurrent(..) => None,
        }).collect()
    }

    fn names_of(xml: &str) -> Vec<String> {
        modifier_fields(xml).into_iter().map(|(name, _, _)| name).collect()
    }

    #[test]
    fn for_each_expression_field_yields_field_names_only() {
        assert_eq!(names_of("<page invisible=\"not partner_id or state == 'draft'\"/>"), ["partner_id", "state"]);
        // Predefined names, the members and the keywords of a call, and literals name no field.
        assert_eq!(names_of("<page invisible=\"uid and context.get('hide')\"/>"), Vec::<String>::new());
        assert_eq!(names_of("<page invisible=\"len(line_ids) &gt; 0\"/>"), ["line_ids"]);
        assert_eq!(names_of("<page invisible=\"bool(context_today(self, tz='UTC'))\"/>"), Vec::<String>::new());
        // `parent.x` belongs to the model above, and a dotted tail is traversal on the first.
        assert_eq!(modifier_fields("<page invisible=\"parent.state\"/>"), [("state".to_string(), 24..29, true)]);
        assert_eq!(modifier_fields("<page invisible=\"parent.line_id.x\"/>"), [("line_id".to_string(), 24..31, true)]);
    }

    /// A domain names fields of the model it targets on the left-hand side of each of its leaves.
    #[test]
    fn for_each_expression_field_yields_the_left_hand_side_of_a_domain_leaf() {
        let names = |xml: &str| target_fields(xml).into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        assert_eq!(names("<filter domain=\"[('state', '=', 'draft')]\"/>"), ["state"]);
        assert_eq!(target_fields("<filter domain=\"[('a.b','=',1)]\"/>"), [("a.b".to_string(), 19..22)]);
        // The operators of a domain, and the values a leaf compares against, name no field.
        assert_eq!(names("<filter domain=\"['|',('a','=',1),('b','in',['c','d'])]\"/>"), ["a", "b"]);
        // A leaf compared against a name is a value of the model in scope rather than a path.
        assert_eq!(names("<filter domain=\"[('a','=',parent_id)]\"/>"), ["a"]);
        assert_eq!(names_of("<filter domain=\"[('a','=',parent_id)]\"/>"), ["parent_id"]);
    }

    #[test]
    fn for_each_expression_field_keeps_offsets_and_skips_what_does_not_parse() {
        // An entity is decoded to keep the expression valid, padded to keep the offsets right.
        assert_eq!(modifier_fields("<page invisible=\"amount &lt; 0\"/>"), [("amount".to_string(), 17..23, false)]);
        // lxml joins the lines of an attribute value, so a broken-up expression is a valid one.
        assert_eq!(names_of("<page invisible=\"state == 'draft'\n    and amount\"/>"), ["state", "amount"]);
        assert!(names_of("<page invisible=\"amount ===\"/>").is_empty());
        assert!(names_of("<page invisible=\"\"/>").is_empty());
    }

    /// Entity + multi-byte char before a `%(...)d` ref used to land mid-character.
    #[test]
    fn for_each_format_xml_id_ref_handles_entities_and_multibyte_chars() {
        let xml = "<button name=\"&lt;\u{1F600}%(module.action_x)d\" type=\"action\"/>";
        let doc = roxmltree::Document::parse(xml).unwrap();
        let node = doc.root_element();
        let attr = node.attributes().next().unwrap();

        let mut hits = vec![];
        XmlAstUtils::for_each_format_xml_id_ref(&attr, xml, |inner, range| {
            hits.push((inner.to_string(), range));
        });

        assert_eq!(hits.len(), 1);
        let (inner, range) = &hits[0];
        assert_eq!(inner, "module.action_x");
        assert!(xml.is_char_boundary(range.start), "range start {} is not a char boundary", range.start);
        assert!(xml.is_char_boundary(range.end), "range end {} is not a char boundary", range.end);
        assert_eq!(&xml[range.clone()], "module.action_x");
    }
}
