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

/// What the attribute value under the cursor expects.
enum XmlTarget {
    /// A model name: `<record model=…>`.
    Model,
    /// A field of that model: `<field name=…>`, `<groupby name=…>`.
    Field(OYarn),
    /// A method of that model: `<button name=…>`.
    Method(OYarn),
    /// An xml id the filter accepts: `ref=`, `groups=`.
    XmlId(XmlIdFilter),
}

pub struct XmlCompletionFeature;

impl XmlCompletionFeature {

    pub fn autocomplete_xml(session: &mut SessionInfo, file_symbol: SourceFileKey, file_info: &Rc<RefCell<FileInfo>>, line: u32, character: u32) -> Option<CompletionResponse> {
        let offset = file_info.borrow().position_to_offset(line, character, session.sync_odoo.encoding);
        let data = file_info.borrow().file_info_ast.borrow().text_document.as_ref()?.contents().to_string();
        let document = match Document::parse(&data) {
            Ok(document) => document,
            Err(_) => {
                warn!("Failed to parse XML document for completion at line {}, character {} in file {}", line, character, file_info.borrow().uri);
                return None;
            }
        };
        let (node, attr) = attribute_at(&document, offset)?;
        let from_module = session.sync_odoo.symbol_table.find_module(file_symbol);
        let scope = XmlAstUtils::scope_at(session, &node, from_module, true);
        let target = target_at(session, &node, &attr, &scope, from_module)?;
        let (span, typed) = completed_span(&data, &attr, offset)?;
        let (mut items, is_incomplete) = build_items(session, file_symbol, from_module, &target, typed);
        let range = file_info.borrow().std_range_to_range(&span, session.sync_odoo.encoding);
        for item in items.iter_mut() {
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range, new_text: item.label.clone() }));
        }
        Some(CompletionResponse::List(CompletionList { is_incomplete, items }))
    }
}

/// Element and attribute whose value the cursor sits in, an empty value being an empty range.
fn attribute_at<'a, 'input>(document: &'a Document<'input>, offset: usize) -> Option<(Node<'a, 'input>, Attribute<'a, 'input>)> {
    for node in document.descendants().filter(|node| node.is_element()) {
        if node.range().end < offset {
            continue;
        }
        for attr in node.attributes() {
            let range = attr.range_value();
            if range.start <= offset && offset <= range.end {
                return Some((node, attr));
            }
        }
    }
    None
}

/// Span the completion replaces and the text typed in it, `groups=` being a list of segments.
fn completed_span<'a>(text: &'a str, attr: &Attribute, offset: usize) -> Option<(Range<usize>, &'a str)> {
    let mut span = attr.range_value();
    if attr.name() == "groups" {
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

/// What `attr` expects, from the tag carrying it and the model in scope.
fn target_at(session: &mut SessionInfo, node: &Node, attr: &Attribute, scope: &XmlScope, from_module: Option<ModuleKey>) -> Option<XmlTarget> {
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
    }
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
        if let Some(current_module) = from_module
            && !model.borrow().model_in_deps(session, current_module)
        {
            if !session.sync_odoo.config.ac_filter_model_names() {
                continue;
            }
            label_details = require_details(session, model.borrow().get_main_symbols(session, None).collect());
        }
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::CLASS),
            label_details,
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
    members.retain(|(name, symbols)| name.starts_with(typed) && !symbols.is_empty());
    members.sort_by(|left, right| left.0.cmp(&right.0));
    members.dedup_by(|left, right| left.0 == right.0);
    let is_incomplete = members.len() > MAX_ITEMS;
    members.truncate(MAX_ITEMS);
    let items = members.into_iter().map(|(name, symbols)| CompletionItem {
        label: name.to_string(),
        kind: Some(match only_methods {
            true => CompletionItemKind::METHOD,
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
