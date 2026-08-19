use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionList, CompletionResponse, CompletionTextEdit, TextEdit};
use roxmltree::{Attribute, Document, Node};
use tracing::warn;

use crate::constants::OYarn;
use crate::core::evaluation::EvaluationSymbolPtr;
use crate::core::evaluation_context::ContextKey;
use crate::core::file_mgr::FileInfo;
use crate::core::odoo::{SyncOdoo, XmlIdFilter};
use crate::core::symbols::storage::SymbolTable;
use crate::core::symbols::storage::xml::xml_field_symbol::XmlFieldName;
use crate::core::symbols::symbol_keys::{ModuleKey, SourceFileKey, SymbolKey};
use crate::features::completion::build_xml_id_item;
use crate::features::xml_ast_utils::{XmlAstUtils, XmlScope};
use crate::threads::SessionInfo;
use crate::oyarn;
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

/// Items an XML completion answers with at most, past which the response is flagged incomplete.
const MAX_ITEMS: usize = 200;

/// Delimiters of the sections the tag scanner steps over, longest opening first.
const TEXT_SECTIONS: [(&str, &str); 4] = [("<!--", "-->"), ("<![CDATA[", "]]>"), ("<?", "?>"), ("<!", ">")];

/// What the attribute value under the cursor expects.
enum XmlTarget {
    /// A model name: `<record model=…>`.
    Model,
    /// A field of that model: `<field name=…>`, `<groupby name=…>`.
    Field(OYarn),
    /// A method of that model: `<button name=…>`.
    Method(OYarn),
    /// An xml id the filter accepts: `ref=`, `groups=`, `<menuitem action=…>`.
    XmlId(XmlIdFilter),
    /// A template name: `<t t-call=…>`, `<template inherit_id=…>`.
    Template,
}

pub struct XmlCompletionFeature;

impl XmlCompletionFeature {

    pub fn autocomplete_xml(session: &mut SessionInfo, file_symbol: SourceFileKey, file_info: &Rc<RefCell<FileInfo>>, line: u32, character: u32) -> Option<CompletionResponse> {
        let offset = file_info.borrow().position_to_offset(line, character, session.sync_odoo.encoding);
        let data = file_info.borrow().file_info_ast.borrow().text_document.as_ref()?.contents().to_string();
        // The tag being typed rarely parses, so a failure is repaired rather than given up on
        let repaired;
        let document = match Document::parse(&data) {
            Ok(document) => document,
            Err(_) => {
                repaired = repaired_prefix(&data, offset)?;
                match Document::parse(&repaired) {
                    Ok(document) => document,
                    Err(_) => {
                        warn!("Failed to parse XML document for completion at line {}, character {} in file {}", line, character, file_info.borrow().uri);
                        return None;
                    }
                }
            }
        };
        let (node, value) = value_at(&document, offset)?;
        let from_module = session.sync_odoo.symbol_table.find_module(file_symbol);
        let scope = XmlAstUtils::scope_at(session, &node, from_module, true);
        let target = target_at(session, &node, &value, &scope, from_module)?;
        let (span, typed) = completed_span(&data, &value, offset)?;
        let (mut items, is_incomplete) = build_items(session, file_symbol, from_module, &target, typed);
        let range = file_info.borrow().std_range_to_range(&span, session.sync_odoo.encoding);
        for item in items.iter_mut() {
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range, new_text: item.label.clone() }));
        }
        Some(CompletionResponse::List(CompletionList { is_incomplete, items }))
    }
}

/// A parsable copy of `text[..offset]`, closing what is half typed without moving any byte.
fn repaired_prefix(text: &str, offset: usize) -> Option<String> {
    let prefix = text.get(..offset)?;
    let mut open_tags: Vec<&str> = vec![];
    let mut index = 0;
    while let Some(found) = prefix[index..].find('<') {
        let start = index + found;
        let rest = &prefix[start..];
        // A comment, a CDATA section or a processing instruction holds text, never markup
        if let Some((open, close)) = TEXT_SECTIONS.iter().find(|(open, _)| rest.starts_with(open)) {
            let Some(end) = rest[open.len()..].find(close) else {
                return Some(close_tags(prefix[..start].to_string(), &open_tags));
            };
            index = start + open.len() + end + close.len();
            continue;
        }
        let is_end = rest.starts_with("</");
        let name_start = 1 + usize::from(is_end);
        let name_len = rest[name_start..].find(['>', '/', ' ', '\t', '\r', '\n']).unwrap_or(rest.len() - name_start);
        let name = &rest[name_start..name_start + name_len];
        let attributes_start = name_start + name_len;
        let mut quote: Option<char> = None;
        let mut tag_end = None;
        for (position, character) in rest[attributes_start..].char_indices() {
            match (quote, character) {
                (Some(open), _) if character == open => quote = None,
                (Some(_), _) => {},
                (None, '"' | '\'') => quote = Some(character),
                (None, '>') => {
                    tag_end = Some(attributes_start + position + 1);
                    break;
                },
                (None, _) => {},
            }
        }
        let Some(tag_end) = tag_end else {
            let mut repaired = prefix[..start].to_string();
            // Only a start tag is filled in, an unfinished end tag is left to the stack to close
            if !is_end && !name.is_empty() {
                repaired.push_str(rest);
                if let Some(quote) = quote {
                    repaired.push(quote);
                } else if rest.trim_end().ends_with('=') {
                    // An attribute whose value the cursor has not opened yet does not parse alone
                    repaired.push_str("\"\"");
                }
                repaired.push_str("/>");
            }
            return Some(close_tags(repaired, &open_tags));
        };
        if is_end {
            if open_tags.last() == Some(&name) {
                open_tags.pop();
            }
        } else if !rest[..tag_end].ends_with("/>") {
            open_tags.push(name);
        }
        index = start + tag_end;
    }
    Some(close_tags(prefix.to_string(), &open_tags))
}

/// `base` followed by an end tag for each element left open in it, innermost first.
fn close_tags(mut base: String, open_tags: &[&str]) -> String {
    for name in open_tags.iter().rev() {
        base.push_str("</");
        base.push_str(name);
        base.push('>');
    }
    base
}

/// A value the cursor may sit in, carried with the element holding it.
enum XmlValue<'a, 'input> {
    /// An attribute value, the quotes around it excluded.
    Attribute(Attribute<'a, 'input>),
    /// The text content of the element, in the byte range the parser gave it.
    Text(Range<usize>),
}

/// Element and value the cursor sits in, an empty one being an empty range.
fn value_at<'a, 'input>(document: &'a Document<'input>, offset: usize) -> Option<(Node<'a, 'input>, XmlValue<'a, 'input>)> {
    let mut empty_content = None;
    for node in document.descendants() {
        if node.range().end < offset {
            continue;
        }
        if node.is_text() && node.range().start <= offset {
            return Some((node.parent()?, XmlValue::Text(node.range())));
        }
        if !node.is_element() {
            continue;
        }
        for attr in node.attributes() {
            let range = attr.range_value();
            if range.start <= offset && offset <= range.end {
                return Some((node, XmlValue::Attribute(attr)));
            }
        }
        // Content the parser gave no text node to, the cursor sitting right after the start tag
        if !node.has_children() && node.range().start < offset && offset < node.range().end
            && document.input_text().get(..offset).is_some_and(|start| start.ends_with('>'))
        {
            empty_content = Some(node);
        }
    }
    empty_content.map(|node| (node, XmlValue::Text(offset..offset)))
}

/// The value in `range` without the whitespace an element spread over several lines pads it with.
fn trimmed_span(text: &str, range: &Range<usize>, offset: usize) -> Range<usize> {
    let Some(value) = text.get(range.clone()) else { return offset..offset };
    let start = range.start + value.len() - value.trim_start().len();
    let end = start + value.trim().len();
    match (start..=end).contains(&offset) {
        true => start..end,
        false => offset..offset,
    }
}

/// Span the completion replaces and the text typed in it, `groups=` being a list of segments.
fn completed_span<'a>(text: &'a str, value: &XmlValue, offset: usize) -> Option<(Range<usize>, &'a str)> {
    let mut span = match value {
        XmlValue::Attribute(attr) => attr.range_value(),
        XmlValue::Text(range) => trimmed_span(text, range, offset),
    };
    if let XmlValue::Attribute(attr) = value && attr.name() == "groups" {
        if let Some(index) = text.get(span.start..offset)?.rfind(',') {
            span.start += index + 1;
        }
        if let Some(index) = text.get(span.start..span.end)?.find(',') {
            span.end = span.start + index;
        }
        let segment = text.get(span.start..offset)?;
        span.start += segment.len() - segment.trim_start_matches([' ', '\t', '!']).len();
    }
    let typed = text.get(span.start..offset)?;
    Some((span, typed))
}

/// What `value` expects, from the tag carrying it and the model in scope.
fn target_at(session: &mut SessionInfo, node: &Node, value: &XmlValue, scope: &XmlScope, from_module: Option<ModuleKey>) -> Option<XmlTarget> {
    let attr = match value {
        XmlValue::Attribute(attr) => attr,
        // The text of a field naming the model a view or an action targets, and nothing else
        XmlValue::Text(_) => return match (node.tag_name().name(), node.attribute("name")?) {
            ("field", "model" | "res_model") => Some(XmlTarget::Model),
            _ => None,
        },
    };
    match (node.tag_name().name(), attr.name()) {
        ("record", "model") => Some(XmlTarget::Model),
        ("field" | "groupby", "name") => Some(XmlTarget::Field(oyarn!("{}", scope.record_model.known()?))),
        // An implicit `type` is `object` in views, `type="action"` takes an xml id instead
        ("button", "name") if node.attribute("type") != Some("action") => {
            Some(XmlTarget::Method(oyarn!("{}", scope.record_model.known()?)))
        },
        ("field", "ref") => {
            let comodel = scope.record_model.known().zip(node.attribute("name"))
                .and_then(|(model, field)| XmlAstUtils::comodel_name(session, model, field, from_module, true));
            Some(XmlTarget::XmlId(match comodel {
                Some(comodel) => XmlIdFilter::Model(oyarn!("{}", comodel)),
                None => XmlIdFilter::Any,
            }))
        },
        (_, "groups") => Some(XmlTarget::XmlId(XmlIdFilter::Model(oyarn!("res.groups")))),
        ("menuitem", "action") => Some(XmlTarget::XmlId(XmlIdFilter::Action)),
        ("menuitem", "parent") => Some(XmlTarget::XmlId(XmlIdFilter::Menu)),
        ("template", "inherit_id") | (_, "t-call" | "t-inherit") => Some(XmlTarget::Template),
        _ => None,
    }
}

/// Completion items for `target`, and whether the cap truncated them.
fn build_items(session: &mut SessionInfo, file_symbol: SourceFileKey, from_module: Option<ModuleKey>, target: &XmlTarget, typed: &str) -> (Vec<CompletionItem>, bool) {
    match target {
        XmlTarget::Model => model_items(session, from_module, typed),
        XmlTarget::Field(model_name) => member_items(session, from_module, model_name, typed, false),
        XmlTarget::Method(model_name) => member_items(session, from_module, model_name, typed, true),
        XmlTarget::XmlId(filter) => xml_id_items(session, file_symbol, from_module, filter, typed),
        XmlTarget::Template => template_items(session, file_symbol, from_module, typed),
    }
}

/// Templates a `t-call` may name: the xml ids of `<template>`, and the frontend `t-name` ones.
fn template_items(session: &mut SessionInfo, file_symbol: SourceFileKey, from_module: Option<ModuleKey>, typed: &str) -> (Vec<CompletionItem>, bool) {
    let (mut items, mut is_incomplete) = xml_id_items(session, file_symbol, from_module, &XmlIdFilter::Template, typed);
    let mut names = session.sync_odoo.js_templates.iter()
        .filter(|(name, templates)| name.starts_with(typed) && templates.iter_valid(session.st()).next().is_some())
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    names.sort();
    names.retain(|name| !items.iter().any(|item| item.label == *name));
    items.extend(names.into_iter().map(|name| CompletionItem {
        label: name,
        kind: Some(CompletionItemKind::REFERENCE),
        ..Default::default()
    }));
    is_incomplete |= items.len() > MAX_ITEMS;
    items.truncate(MAX_ITEMS);
    (items, is_incomplete)
}

fn xml_id_items(session: &mut SessionInfo, file_symbol: SourceFileKey, from_module: Option<ModuleKey>, filter: &XmlIdFilter, typed: &str) -> (Vec<CompletionItem>, bool) {
    let mut candidates = SyncOdoo::get_xml_ids_by_prefix(session, file_symbol, typed, filter);
    candidates.sort_by_cached_key(|(module_key, local_id, _)| (session.st()[*module_key].dir_name.clone(), local_id.clone()));
    let is_incomplete = candidates.len() > MAX_ITEMS;
    candidates.truncate(MAX_ITEMS);
    let items = candidates.iter()
        .filter_map(|(module_key, local_id, in_deps)| build_xml_id_item(session, from_module, *module_key, local_id, *in_deps))
        .collect();
    (items, is_incomplete)
}

fn model_items(session: &mut SessionInfo, from_module: Option<ModuleKey>, typed: &str) -> (Vec<CompletionItem>, bool) {
    let mut names = session.sync_odoo.models.keys()
        .filter(|name| name.starts_with(typed) && *name != "_unknown")
        .cloned()
        .collect::<Vec<_>>();
    names.sort();
    let is_incomplete = names.len() > MAX_ITEMS;
    names.truncate(MAX_ITEMS);
    let mut items = vec![];
    for name in names {
        let Some(model) = session.sync_odoo.models.get(&name).cloned() else { continue };
        if !model.borrow().has_symbols(session.st()) {
            continue;
        }
        let mut label_details = None;
        // `_` sorts before every model name, putting the ones usable as they are first
        let mut sort_text = format!("_{name}");
        if let Some(current_module) = from_module
            && !model.borrow().model_in_deps(session, current_module)
        {
            if !session.sync_odoo.config.ac_filter_model_names() {
                continue;
            }
            label_details = require_details(session, model.borrow().get_main_symbols(session, None).collect());
            sort_text = name.to_string();
        }
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::CLASS),
            label_details,
            sort_text: Some(sort_text),
            ..Default::default()
        });
    }
    (items, is_incomplete)
}

/// The `require <module>` note carried by a candidate out of the current dependencies.
fn require_details(session: &SessionInfo, symbols: Vec<impl Into<SymbolKey> + Copy>) -> Option<CompletionItemLabelDetails> {
    let mut dep_names = symbols.iter()
        .filter_map(|&symbol| session.st().find_module(symbol.into()))
        .map(|module| session.st()[module].dir_name.to_string())
        .collect::<Vec<_>>();
    dep_names.sort();
    dep_names.dedup();
    if dep_names.is_empty() {
        return None;
    }
    Some(CompletionItemLabelDetails {
        detail: None,
        description: Some(format!("require {}", dep_names.join(", "))),
    })
}

fn member_items(session: &mut SessionInfo, from_module: Option<ModuleKey>, model_name: &OYarn, typed: &str, only_methods: bool) -> (Vec<CompletionItem>, bool) {
    let Some(model) = session.sync_odoo.models.get(model_name).cloned() else { return (vec![], false) };
    let mut members: Vec<(OYarn, Vec<SymbolKey>)> = vec![];
    let main_symbol = model.borrow().get_main_symbols(session, from_module).next();
    if let Some(main_symbol) = main_symbol {
        members.extend(SymbolTable::all_members(main_symbol.into(), session, true, !only_methods, only_methods, from_module, false));
    }
    if !only_methods {
        // Fields declared as `ir.model.fields` records live outside of any python class.
        let xml_fields = model.borrow().get_xml_model_field_symbols(session.st(), from_module).collect::<Vec<_>>();
        for record_key in xml_fields {
            if let Some(name) = session.st()[record_key].get_field_text(XmlFieldName::Name, session.st()) {
                members.push((oyarn!("{}", name), vec![record_key.into()]));
            }
        }
    }
    members.retain(|(name, symbols)| {
        // A name in angle brackets is synthetic, `<lambda>` and the like, and cannot be typed
        name.starts_with(typed) && !symbols.is_empty() && !(name.starts_with('<') && name.ends_with('>'))
    });
    // Sorted on the rank rather than the name so that the cap keeps the public members
    members.sort_by_cached_key(|(name, _)| member_sort_text(name));
    members.dedup_by(|left, right| left.0 == right.0);
    let is_incomplete = members.len() > MAX_ITEMS;
    members.truncate(MAX_ITEMS);
    let items = members.into_iter().map(|(name, symbols)| CompletionItem {
        sort_text: Some(member_sort_text(&name)),
        label: name.to_string(),
        kind: Some(match only_methods {
            // A callable kind makes the client insert parentheses, which no attribute value takes
            true => CompletionItemKind::VALUE,
            false => CompletionItemKind::FIELD,
        }),
        label_details: match only_methods {
            true => None,
            false => field_details(session, symbols[0]).map(|description| CompletionItemLabelDetails {
                detail: None,
                description: Some(description),
            }),
        },
        ..Default::default()
    }).collect();
    (items, is_incomplete)
}

/// Rank of a member, `_private` after the public ones and `__dunder__` after those.
fn member_sort_text(name: &str) -> String {
    let mut text = name.to_string();
    if name.starts_with('_') {
        text.insert(0, '~');
    }
    if name.starts_with("__") {
        text.insert(0, '~');
    }
    text
}

/// Type of a field as `Many2one(res.partner)` or `Char`, for the note beside its label.
fn field_details(session: &mut SessionInfo, symbol: SymbolKey) -> Option<String> {
    match symbol {
        SymbolKey::Variable(variable_key) => {
            for eval in session.st()[variable_key].evaluations.clone().iter() {
                let ptr = eval.symbol.get_symbol(session, None, &mut vec![], None);
                let comodel = match &ptr {
                    EvaluationSymbolPtr::WEAK(weak) => weak.context.get(ContextKey::ComodelName).map(|value| value.as_str().to_string()),
                    _ => None,
                };
                for followed in SymbolTable::follow_ref(&ptr, session, None, true, false, None, None).iter() {
                    let Some(class_key) = followed.upgrade_weak(session.st()) else { continue };
                    if !SymbolTable::is_field_class(session, class_key) {
                        continue;
                    }
                    let ttype = session.sync_odoo.get_main_entry_tree(class_key).flatten().last()?.to_string();
                    return Some(match comodel {
                        Some(comodel) => format!("{ttype}({comodel})"),
                        None => ttype,
                    });
                }
            }
            None
        },
        SymbolKey::XmlRecord(record_key) => {
            let ttype = session.st()[record_key].get_field_text(XmlFieldName::Type, session.st())?;
            match session.st()[record_key].get_field_text(XmlFieldName::Relation, session.st()) {
                Some(relation) => Some(format!("{ttype}({relation})")),
                None => Some(ttype),
            }
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repaired(text: &str) -> String {
        repaired_prefix(text, text.len()).unwrap()
    }

    #[test]
    fn repaired_prefix_closes_the_tag_being_typed() {
        assert_eq!(
            repaired(r#"<odoo><record model="ir.ui.view"><field name="i"#),
            r#"<odoo><record model="ir.ui.view"><field name="i"/></record></odoo>"#
        );
        assert_eq!(
            repaired("<odoo><form><sheet><group><field name="),
            r#"<odoo><form><sheet><group><field name=""/></group></sheet></form></odoo>"#
        );
    }

    /// A `>` or a quote inside an attribute value never ends the tag it belongs to.
    #[test]
    fn repaired_prefix_reads_quoted_attribute_values() {
        assert_eq!(
            repaired(r#"<odoo><field invisible="a > 1" readonly="b != 'c'"/><field name="#),
            r#"<odoo><field invisible="a > 1" readonly="b != 'c'"/><field name=""/></odoo>"#
        );
    }

    /// Markup quoted in a comment, a CDATA section or a processing instruction is only text.
    #[test]
    fn repaired_prefix_steps_over_text_sections() {
        assert_eq!(
            repaired(r#"<odoo><!-- <record model="x"> --><field name="a"#),
            r#"<odoo><!-- <record model="x"> --><field name="a"/></odoo>"#
        );
        assert_eq!(
            repaired(r#"<?xml version="1.0"?><odoo><![CDATA[<form>]]><field name="a"#),
            r#"<?xml version="1.0"?><odoo><![CDATA[<form>]]><field name="a"/></odoo>"#
        );
    }

    /// An unfinished end tag is dropped, the stack closes the element it was going to close.
    #[test]
    fn repaired_prefix_drops_an_unfinished_end_tag() {
        assert_eq!(repaired("<odoo><form></fo"), "<odoo><form></form></odoo>");
    }

    /// Only the text up to the cursor is kept, whatever follows it.
    #[test]
    fn repaired_prefix_cuts_at_the_cursor() {
        let text = r#"<odoo><field name="amount"/></odoo>"#;
        let offset = text.find("amount").unwrap() + 2;
        assert_eq!(repaired_prefix(text, offset).unwrap(), r#"<odoo><field name="am"/></odoo>"#);
    }
}
