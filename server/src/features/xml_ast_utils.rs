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
    threads::SessionInfo,
};
use roxmltree::{Attribute, Node};
use std::{ops::Range, rc::Rc};

/// Tags making a `<field>`'s content an inline subview, `tree` being pre-18.0 for `list`.
const SUBVIEW_TAGS: [&str; 6] = ["form", "list", "tree", "graph", "kanban", "calendar"];

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
    /// `name` of the enclosing `<field>` (drives `<field name="model">` text).
    pub field_name: Option<&'a str>,
    /// For an `ir.ui.view` record, the model its arch targets (captured from the
    /// record's `<field name="model">`). Applied to the `<field name="arch">` subtree.
    pub view_target_model: Option<&'a str>,
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
            } else if attr.name() == "groups"
            && let Some(file_module) = from_module
            {
                XmlAstUtils::emit_xml_id(session, XmlRefKind::XmlId, attr.value(), file_module, attr.range_value(), out, on_dep_only);
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
            child_scope.record_model = model_scope;
        }
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module, &child_scope, out, on_dep_only);
        }
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
    fn comodel_name(session: &mut SessionInfo, model_name: &str, field_name: &str, from_module: Option<ModuleKey>, on_dep_only: bool) -> Option<Rc<str>> {
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
        XmlAstUtils::emit_attribute_xml_ids(session, node, offset, from_module, &["action", "groups"], out, on_dep_only);
        for child in node.children() {
            XmlAstUtils::visit_node(session, &child, offset, from_module, scope, out, on_dep_only);
        }
    }

    fn visit_template<'a>(session: &mut SessionInfo<'_>, node: &Node<'a, '_>, offset: Option<usize>, from_module: Option<ModuleKey>, scope: &XmlScope<'a>, out: &mut dyn FnMut(XmlRef), on_dep_only: bool) {
        XmlAstUtils::emit_attribute_xml_ids(session, node, offset, from_module, &["inherit_id", "groups"], out, on_dep_only);
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
            if let XmlId::XmlRecord(record_key) = xml_id {
                symbols.push(record_key.into());
            } else if let XmlId::PythonClass(record_key) = xml_id {
                symbols.push(record_key.into());
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

#[cfg(test)]
mod tests {
    use super::*;

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
