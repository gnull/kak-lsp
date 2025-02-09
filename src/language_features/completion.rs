use crate::context::*;
use crate::markup::*;
use crate::position::*;
use crate::text_edit::apply_text_edits;
use crate::types::*;
use crate::util::*;
use indoc::formatdoc;
use itertools::Itertools;
use lazy_static::lazy_static;
use lsp_types::request::*;
use lsp_types::*;
use regex::Regex;
use serde::Deserialize;
use std::convert::TryInto;
use unicode_width::UnicodeWidthStr;
use url::Url;

pub fn text_document_completion(meta: EditorMeta, params: EditorParams, ctx: &mut Context) {
    let params = TextDocumentCompletionParams::deserialize(params).unwrap();
    let req_params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::from_file_path(&meta.buffile).unwrap(),
            },
            position: get_lsp_position(&meta.buffile, &params.position, ctx).unwrap(),
        },
        context: None,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    ctx.call::<Completion, _>(meta, req_params, |ctx: &mut Context, meta, result| {
        editor_completion(meta, params, result, ctx)
    });
}

fn editor_completion(
    meta: EditorMeta,
    params: TextDocumentCompletionParams,
    result: Option<CompletionResponse>,
    ctx: &mut Context,
) {
    let items = match result {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(list)) => list.items,
        None => vec![],
    };

    let version = meta.version;
    ctx.completion_items = items;
    ctx.completion_items_timestamp = version;
    let items = &ctx.completion_items;
    if ctx.completion_last_client != meta.client {
        ctx.completion_last_client = meta.client.clone();
    }

    if items.is_empty() {
        return;
    }

    // Maximum display width of any completion label.
    let maxwidth = items
        .iter()
        .map(|x| UnicodeWidthStr::width(x.label.as_str()))
        .max()
        .unwrap_or(0);

    let mut inferred_offset: Option<u32> = None;
    let mut can_infer_offset = true;

    let items = items
        .iter()
        .enumerate()
        .map(|(completion_item_index, x)| {
            let maybe_resolve = if ctx
                .capabilities
                .as_ref()
                .and_then(|caps| caps.completion_provider.as_ref())
                .and_then(|compl| compl.resolve_provider)
                .unwrap_or(false)
            {
                "lsp-completion-item-resolve\n"
            } else {
                ""
            };
            let on_select = formatdoc!(
                "lsp-completion-item-selected {completion_item_index}
                 {maybe_resolve}info -markup -style menu -- %§{}§",
                completion_menu_text(x).replace('§', "§§")
            );

            let entry = match x.kind {
                Some(k) => format!(
                    "{}{} {{MenuInfo}}{:?}",
                    escape_kakoune_markup(&x.label),
                    " ".repeat(maxwidth - UnicodeWidthStr::width(x.label.as_str())),
                    k
                ),
                None => escape_kakoune_markup(&x.label),
            };

            let insert_text = x.text_edit.as_ref().and_then(|cte| {
                let document = match ctx.documents.get(&meta.buffile) {
                    Some(doc) => doc,
                    None => {
                        warn!("No document in context for file: {}", &meta.buffile);
                        can_infer_offset = false;
                        return None;
                    }
                };

                match cte {
                    CompletionTextEdit::Edit(text_edit) => {
                        // The generic textEdit property is not supported yet (#40).  However,
                        // we can support simple text edits that only replace the token left
                        // of the cursor. Kakoune will do this very edit if we simply pass it
                        // the replacement string as completion.
                        let range = lsp_range_to_kakoune(
                            &text_edit.range,
                            &document.text,
                            ctx.offset_encoding,
                        );

                        if can_infer_offset {
                            match inferred_offset {
                                None => inferred_offset = Some(range.start.column),
                                Some(offset) if offset != range.start.column => {
                                    can_infer_offset = false;
                                    inferred_offset = None
                                }
                                _ => (),
                            }
                        };

                        if range.start.line == params.position.line
                            && range.end.line == params.position.line
                            // Not sure why this case happens, see #455
                            && (range.end.column == params.position.column
                                || range.end.column + 1 == params.position.column)
                        {
                            Some(text_edit.new_text.clone())
                        } else {
                            None
                        }
                    }
                    CompletionTextEdit::InsertAndReplace(_) => {
                        can_infer_offset = false;
                        None
                    }
                }
            });
            let insert_text = insert_text
                .or_else(|| x.insert_text.clone())
                .unwrap_or_else(|| x.label.clone());

            fn completion_entry(
                insert_text: &str,
                maybe_filter_text: &Option<String>,
                on_select: &str,
                menu: &str,
            ) -> String {
                if let Some(filter_text) = maybe_filter_text {
                    editor_quote(&format!(
                        "{}|{}|{}|{}",
                        escape_tuple_element(insert_text),
                        escape_tuple_element(filter_text),
                        escape_tuple_element(on_select),
                        escape_tuple_element(menu),
                    ))
                } else {
                    editor_quote(&format!(
                        "{}|{}|{}",
                        escape_tuple_element(insert_text),
                        escape_tuple_element(on_select),
                        escape_tuple_element(menu),
                    ))
                }
            }

            let maybe_filter_text = {
                let specified_filter_text = x.filter_text.as_ref().unwrap_or(&x.label);
                let specified_insert_text = x
                    .text_edit
                    .as_ref()
                    .map(|cte| match cte {
                        CompletionTextEdit::Edit(text_edit) => &text_edit.new_text,
                        CompletionTextEdit::InsertAndReplace(text_edit) => &text_edit.new_text,
                    })
                    .or(x.insert_text.as_ref())
                    .unwrap_or(&x.label);
                if !params.have_kakoune_feature_filtertext
                    && specified_filter_text != specified_insert_text
                    && x.insert_text_format != Some(InsertTextFormat::SNIPPET)
                {
                    // Simulate filter-text support by giving the filter-text to Kakoune
                    // but expand to the insert-text when the completion is accepted.
                    let command = formatdoc!(
                        "{on_select}
                         lsp-snippets-insert-completion {}",
                        editor_quote(&(insert_text + "$0"))
                    );
                    let insert_text = specified_filter_text;
                    return completion_entry(insert_text, &None, &command, &entry);
                }
                if params.have_kakoune_feature_filtertext
                    && specified_filter_text != specified_insert_text
                {
                    Some(specified_filter_text.clone())
                } else {
                    None
                }
            };

            // If snippet support is both enabled and provided by the server,
            // we'll need to perform some transformations on the completion commands.
            if ctx.config.snippet_support && x.insert_text_format == Some(InsertTextFormat::SNIPPET)
            {
                lazy_static! {
                    static ref SNIPPET_TABSTOP_RE: Regex = Regex::new(r"\$(?P<i>\d+)").unwrap();
                    static ref SNIPPET_PLACEHOLDER_RE: Regex =
                        Regex::new(r"\$\{(?P<i>\d+):?(?P<placeholder>[^}]+)\}").unwrap();
                }
                let mut snippet = insert_text;
                if !snippet.contains("$0") && !snippet.contains("${0") {
                    snippet += "$0";
                }
                let insert_text = SNIPPET_TABSTOP_RE.replace_all(&snippet, "");
                let insert_text = SNIPPET_PLACEHOLDER_RE.replace_all(&insert_text, "$placeholder");
                // There's some issue with multiline insert texts, and they also don't work well in the UI, so display on one line
                let insert_text = insert_text.replace('\n', "");

                let command = formatdoc!(
                    "{on_select}
                     lsp-snippets-insert-completion {}",
                    editor_quote(&snippet)
                );

                completion_entry(&insert_text, &maybe_filter_text, &command, &entry)
            } else {
                completion_entry(&insert_text, &maybe_filter_text, &on_select, &entry)
            }
        })
        .join(" ");

    let line = params.position.line;
    let offset = inferred_offset.unwrap_or(params.completion.offset);
    let command = formatdoc!(
        "set-option window lsp_completions {line}.{offset}@{version} {items}
         set-option window lsp_completions_timestamp {version}"
    );
    let command = format!("evaluate-commands -- {}", editor_quote(&command));

    ctx.exec(meta, command);
}

fn completion_menu_text(x: &CompletionItem) -> String {
    // Combine the 'detail' line and the full-text documentation into
    // a single string. If both exist, separate them with a horizontal rule.
    let mut markup = String::new();

    if let Some(detail) = x.detail.as_ref() {
        markup.push_str(&escape_kakoune_markup(detail));

        if x.documentation.is_some() {
            markup.push_str("\n\n---\n\n");
        }
    }

    match x.documentation.as_ref() {
        Some(Documentation::String(s)) => markup.push_str(&escape_kakoune_markup(s)),
        Some(Documentation::MarkupContent(content)) => match content.kind {
            MarkupKind::PlainText => markup.push_str(&escape_kakoune_markup(&content.value)),
            MarkupKind::Markdown => markup.push_str(&markdown_to_kakoune_markup(&content.value)),
        },
        _ => (),
    }

    markup
}

pub fn completion_item_resolve(meta: EditorMeta, params: EditorParams, ctx: &mut Context) {
    let CompletionItemResolveParams {
        completion_item_timestamp,
        completion_item_index,
        pager_active,
    } = CompletionItemResolveParams::deserialize(params).unwrap();

    if ctx.completion_last_client.is_none() || meta.client != ctx.completion_last_client {
        return;
    }

    if completion_item_timestamp != ctx.completion_items_timestamp {
        return;
    }

    if completion_item_index >= ctx.completion_items.len().try_into().unwrap() {
        error!(
            "ignoring request to resolve completion item of invalid index {completion_item_index}"
        );
        return;
    }

    let (item, detail, documentation) = if pager_active {
        let item = &ctx.completion_items[completion_item_index as usize];
        // Stop if there is nothing interesting to resolve.
        if item.detail.is_some() && item.documentation.is_some() {
            return;
        }
        (
            item.clone(),
            item.detail.clone(),
            item.documentation.clone(),
        )
    } else {
        // Since we're the only user of the completion items, we can clear them.
        let item = ctx
            .completion_items
            .drain(..)
            .nth(completion_item_index as usize)
            .unwrap();

        match item.additional_text_edits {
            Some(edits) if !edits.is_empty() => {
                // Not sure if this case ever happens, the spec is unclear.
                let uri = Url::from_file_path(&meta.buffile).unwrap();
                apply_text_edits(&meta, &uri, edits, ctx);
                return;
            }
            _ => (),
        }

        (item, None, None)
    };

    ctx.call::<ResolveCompletionItem, _>(meta, item, move |tx: &mut Context, meta, new_item| {
        editor_completion_item_resolve(tx, meta, pager_active, detail, documentation, new_item)
    });
}

fn editor_completion_item_resolve(
    ctx: &mut Context,
    meta: EditorMeta,
    pager_active: bool,
    old_detail: Option<String>,
    old_documentation: Option<Documentation>,
    new_item: CompletionItem,
) {
    if pager_active {
        if new_item.detail == old_detail || new_item.documentation == old_documentation {
            return;
        }
        ctx.exec(
            meta,
            format!(
                "info -markup -style menu -- %§{}§",
                completion_menu_text(&new_item).replace('§', "§§")
            ),
        );
    } else if let Some(resolved_edits) = new_item.additional_text_edits {
        let uri = Url::from_file_path(&meta.buffile).unwrap();
        apply_text_edits(&meta, &uri, resolved_edits, ctx)
    }
}
