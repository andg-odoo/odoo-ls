use std::cell::RefCell;
use std::rc::Rc;

use lsp_types::{Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation};
use ruff_python_ast::ExprCall;
use ruff_text_size::{Ranged, TextSize};

use crate::constants::{OYarn, SymType};
use crate::oyarn;
use crate::core::evaluation::{Evaluation, EvaluationSymbolPtr};
use crate::core::evaluation_context::{ContextKey, ContextValue};
use crate::core::file_mgr::FileInfo;
use crate::core::symbols::storage::function_symbol::{Argument, ArgumentType};
use crate::core::symbols::symbol_keys::{FunctionKey, SourceFileKey, SymbolKey};
use crate::core::symbols::SymbolTable;
use crate::features::ast_utils::{AstUtils, ExprFinderVisitor};
use crate::features::features_utils::{CallableSignature, FeaturesUtils, TypeInfo};
use crate::threads::SessionInfo;

/// Which parameter of the call the cursor sits on, expressed in call terms.
enum CallCursor {
    Positional(usize),
    Keyword(OYarn),
}

/// A parameter as displayed in the signature label, matched against [`CallCursor`].
struct DisplayedArg {
    name: OYarn,
    arg_type: ArgumentType,
    label: String,
}

pub struct SignatureHelpFeature {}

impl SignatureHelpFeature {

    pub fn signature_help_python(session: &mut SessionInfo, file_symbol: SourceFileKey, file_info: &Rc<RefCell<FileInfo>>, line: u32, character: u32) -> Option<SignatureHelp> {
        let offset = file_info.borrow().position_to_offset(line, character, session.sync_odoo.encoding) as u32;
        let file_info_ast_clone = file_info.borrow().file_info_ast.clone();
        //release the ast borrow before evaluating: resolving the callee can rebuild the file
        let call_expr = {
            let file_info_ast_ref = file_info_ast_clone.borrow();
            file_info_ast_ref.get_stmts()?.iter()
                .find_map(|stmt| ExprFinderVisitor::find_expr_at(stmt, offset).1)?
        };
        let cursor = SignatureHelpFeature::locate_cursor(&call_expr, TextSize::new(offset));
        let signatures = SignatureHelpFeature::build_signatures(session, file_symbol, &call_expr, offset, &cursor);
        if signatures.is_empty() {
            return None;
        }
        Some(SignatureHelp {
            //also set at the top level, for clients that do not read it on the signature itself
            active_parameter: signatures[0].active_parameter,
            signatures,
            active_signature: Some(0),
        })
    }

    /// Find the parameter the cursor is on, counting closed arguments when it is between two.
    fn locate_cursor(call_expr: &ExprCall, offset: TextSize) -> CallCursor {
        let args = &call_expr.arguments.args;
        let keywords = &call_expr.arguments.keywords;
        if let Some(index) = args.iter().position(|arg| arg.range().contains_inclusive(offset)) {
            return CallCursor::Positional(index);
        }
        if let Some(keyword) = keywords.iter().find(|keyword| keyword.range().contains_inclusive(offset)) {
            match keyword.arg.as_ref() {
                Some(name) => return CallCursor::Keyword(oyarn!("{}", name.id)),
                None => return CallCursor::Positional(args.len()), //dict unpacking, no name to match on
            }
        }
        CallCursor::Positional(
            args.iter().map(Ranged::range).chain(keywords.iter().map(Ranged::range))
                .filter(|range| range.end() < offset).count()
        )
    }

    fn build_signatures(session: &mut SessionInfo, file_symbol: SourceFileKey, call_expr: &ExprCall, offset: u32, cursor: &CallCursor) -> Vec<SignatureInformation> {
        let scope = session.st().get_scope_symbol(file_symbol, offset, false);
        let from_module = session.st().find_module(file_symbol);
        AstUtils::build_scope(session, scope);
        let callable_evals = Evaluation::eval_from_ast(session, &call_expr.func, scope, &call_expr.func.range().start(), false, &mut vec![]).0;
        let callable_eval_sym_ptrs = callable_evals.iter().flat_map(|callable_eval|
            SymbolTable::follow_ref(&callable_eval.symbol.get_symbol(session, None, &mut vec![], None), session, None, false, false, None, None)
        ).collect::<Vec<_>>();
        let mut signatures = vec![];
        for callable_eval in callable_eval_sym_ptrs.iter() {
            let EvaluationSymbolPtr::WEAK(callable) = callable_eval else {
                continue;
            };
            let Some(callable_sym) = callable.weak.upgrade(session.st()) else {continue};
            let (func_key, is_on_instance) = match callable_sym.typ() {
                SymType::CLASS => {
                    let Some(&SymbolKey::Function(init_method)) = SymbolTable::get_member_symbol(
                        session, callable_sym, "__init__", from_module, false, false, true, false, false).0.first() else {
                        continue;
                    };
                    (init_method, true)
                },
                SymType::FUNCTION => (
                    callable_sym.unwrap_function_key(),
                    callable.context.get(ContextKey::IsAttrOfInstance).map(ContextValue::as_bool).unwrap_or(false)
                ),
                _ => continue,
            };
            let signature = SignatureHelpFeature::build_signature(session, callable_eval, func_key, is_on_instance, cursor);
            if !signatures.contains(&signature) {
                signatures.push(signature);
            }
        }
        signatures
    }

    fn build_signature(session: &mut SessionInfo, callable_eval: &EvaluationSymbolPtr, func_key: FunctionKey, is_on_instance: bool, cursor: &CallCursor) -> SignatureInformation {
        //the bound argument (self/cls) is not part of the call, hide it
        let skipped = usize::from(is_on_instance && !session.st()[func_key].is_static);
        let args = session.st()[func_key].args.clone();
        let displayed_args = args.iter().skip(skipped)
            .map(|arg| SignatureHelpFeature::displayed_arg(session, arg)).collect::<Vec<_>>();
        //a class callable evaluates to the class itself, which is what __init__ returns
        let return_type = match FeaturesUtils::get_inferred_types(session, callable_eval, None, &SymType::FUNCTION) {
            TypeInfo::CALLABLE(CallableSignature { return_types, .. }) => return_types,
            TypeInfo::VALUE(value) => value,
        };
        let mut label = format!("{}(", session.st()[func_key].name);
        let mut parameters = vec![];
        for (index, displayed_arg) in displayed_args.iter().enumerate() {
            if index > 0 {
                label += ", ";
            }
            let start = label.encode_utf16().count() as u32;
            label += &displayed_arg.label;
            parameters.push(ParameterInformation {
                label: ParameterLabel::LabelOffsets([start, label.encode_utf16().count() as u32]),
                documentation: None,
            });
        }
        label += &format!(") -> {return_type}");
        SignatureInformation {
            label,
            documentation: session.st()[func_key].doc_string.as_ref().map(|doc_string| Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: doc_string.clone(),
            })),
            parameters: Some(parameters),
            active_parameter: SignatureHelpFeature::active_parameter(&displayed_args, cursor),
        }
    }

    fn displayed_arg(session: &mut SessionInfo, arg: &Argument) -> DisplayedArg {
        let presentation = FeaturesUtils::argument_presentation(session, arg);
        DisplayedArg {
            name: arg.symbol.upgrade(session.st()).map(|symbol| session.st()[symbol].name.clone()).unwrap_or_default(),
            label: match arg.arg_type {
                ArgumentType::VARARG => format!("*{presentation}"),
                ArgumentType::KWARG => format!("**{presentation}"),
                _ => presentation,
            },
            arg_type: arg.arg_type.clone(),
        }
    }

    /// Index of the displayed parameter under the cursor, overflow landing on `*args`/`**kwargs`.
    fn active_parameter(displayed_args: &[DisplayedArg], cursor: &CallCursor) -> Option<u32> {
        let position_of = |arg_type: ArgumentType| displayed_args.iter().position(|displayed_arg| displayed_arg.arg_type == arg_type);
        let index = match cursor {
            CallCursor::Keyword(name) => displayed_args.iter()
                .position(|displayed_arg| displayed_arg.name == *name && !matches!(displayed_arg.arg_type, ArgumentType::VARARG | ArgumentType::POS_ONLY))
                .or_else(|| position_of(ArgumentType::KWARG)),
            CallCursor::Positional(index) => match displayed_args.get(*index) {
                Some(displayed_arg) if !matches!(displayed_arg.arg_type, ArgumentType::KWORD_ONLY | ArgumentType::KWARG) => Some(*index),
                _ => position_of(ArgumentType::VARARG),
            },
        };
        index.map(|index| index as u32)
    }
}
