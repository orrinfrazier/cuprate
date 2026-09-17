use std::collections::{BTreeMap, BTreeSet};

use pretty_assertions::assert_eq;
use syn::{
    visit::{self, Visit},
    Expr, ExprCall, ExprMatch, Item, Macro,
};

/// A handler source file. `tag` is a short module label ("`json_rpc`", "`other_json`", "bin", "shared");
/// `src` is the full Rust source text of that file.
struct HandlerFile {
    tag: &'static str,
    src: &'static str,
}

/// Returns (`dispatch_file_tag`, `handler_fn_name`) for every handler that is *live-dispatched* from a
/// `map_request` match arm AND whose body transitively (following calls to fns defined in the
/// supplied files, e.g. `shared::foo`) contains `todo!`/`unimplemented!`.
///
/// "Live-dispatched" = a match arm whose RHS calls a handler fn other than `not_available`. Arms
/// that call `not_available()` or `return Err(...)` are NOT dispatched and contribute nothing.
fn dispatched_todo_violations(files: &[HandlerFile]) -> BTreeSet<(String, String)> {
    let parsed_files = files
        .iter()
        .map(|file| ParsedHandlerFile {
            tag: file.tag.to_owned(),
            ast: syn::parse_file(file.src).unwrap_or_else(|error| {
                panic!("parse {}: {error}", file.tag);
            }),
        })
        .collect::<Vec<_>>();

    let mut table = BTreeMap::new();

    for file in &parsed_files {
        for item in &file.ast.items {
            if let Item::Fn(item_fn) = item {
                let mut visitor = FnBodyVisitor::default();
                visitor.visit_block(&item_fn.block);

                table.insert(
                    (file.tag.clone(), item_fn.sig.ident.to_string()),
                    FnInfo {
                        has_direct_todo: visitor.has_direct_todo,
                        calls: visitor.calls,
                    },
                );
            }
        }
    }

    let mut live_dispatched = BTreeSet::new();

    for file in &parsed_files {
        for item in &file.ast.items {
            if let Item::Fn(item_fn) = item {
                if item_fn.sig.ident != "map_request" {
                    continue;
                }

                let mut first_match = FirstMatchVisitor::default();
                first_match.visit_block(&item_fn.block);

                if let Some(expr_match) = first_match.selected_match() {
                    for arm in &expr_match.arms {
                        let mut collector = CallCollector::default();
                        collector.visit_expr(&arm.body);

                        for call in collector.calls {
                            if call.1 == "not_available" {
                                continue;
                            }

                            if let Some((resolved_module, resolved_name)) =
                                resolve_call(file.tag.as_str(), &call, &table)
                            {
                                live_dispatched.insert(LiveDispatch {
                                    dispatch_tag: file.tag.clone(),
                                    resolved_module,
                                    resolved_name,
                                });
                            }
                        }
                    }
                }

                break;
            }
        }
    }

    let mut tainted = BTreeSet::new();

    loop {
        let mut changed = false;

        for (key, info) in &table {
            if tainted.contains(key) {
                continue;
            }

            let is_tainted = info.has_direct_todo
                || info
                    .calls
                    .iter()
                    .filter_map(|call| resolve_call(&key.0, call, &table))
                    .any(|resolved| tainted.contains(&resolved));

            if is_tainted && tainted.insert(key.clone()) {
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    live_dispatched
        .into_iter()
        .filter(|dispatch| {
            tainted.contains(&(
                dispatch.resolved_module.clone(),
                dispatch.resolved_name.clone(),
            ))
        })
        .map(|dispatch| (dispatch.dispatch_tag, dispatch.resolved_name))
        .collect()
}

struct ParsedHandlerFile {
    tag: String,
    ast: syn::File,
}

struct FnInfo {
    has_direct_todo: bool,
    calls: BTreeSet<(Option<String>, String)>,
}

#[derive(Ord, PartialOrd, Eq, PartialEq)]
struct LiveDispatch {
    dispatch_tag: String,
    resolved_module: String,
    resolved_name: String,
}

#[derive(Default)]
struct FnBodyVisitor {
    has_direct_todo: bool,
    calls: BTreeSet<(Option<String>, String)>,
}

impl<'ast> Visit<'ast> for FnBodyVisitor {
    fn visit_macro(&mut self, mac: &'ast Macro) {
        if is_todo_macro(mac) {
            self.has_direct_todo = true;
        }

        visit::visit_macro(self, mac);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Some(path_call) = path_call(call) {
            self.calls.insert(path_call);
        }

        visit::visit_expr_call(self, call);
    }
}

#[derive(Default)]
struct CallCollector {
    calls: BTreeSet<(Option<String>, String)>,
}

impl<'ast> Visit<'ast> for CallCollector {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Some(path_call) = path_call(call) {
            self.calls.insert(path_call);
        }

        visit::visit_expr_call(self, call);
    }
}

#[derive(Default)]
struct FirstMatchVisitor<'ast> {
    first_match: Option<&'ast ExprMatch>,
    request_match: Option<&'ast ExprMatch>,
}

impl<'ast> Visit<'ast> for FirstMatchVisitor<'ast> {
    fn visit_expr_match(&mut self, expr_match: &'ast ExprMatch) {
        if self.first_match.is_none() {
            self.first_match = Some(expr_match);
        }

        if self.request_match.is_none() && is_request_match(expr_match) {
            self.request_match = Some(expr_match);
        }

        visit::visit_expr_match(self, expr_match);
    }
}

impl<'ast> FirstMatchVisitor<'ast> {
    fn selected_match(&self) -> Option<&'ast ExprMatch> {
        self.request_match.or(self.first_match)
    }
}

fn is_todo_macro(mac: &Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "todo" || segment.ident == "unimplemented")
}

fn is_request_match(expr_match: &ExprMatch) -> bool {
    let Expr::Path(path) = expr_match.expr.as_ref() else {
        return false;
    };

    path.path
        .get_ident()
        .is_some_and(|ident| ident == "request")
}

fn path_call(call: &ExprCall) -> Option<(Option<String>, String)> {
    let Expr::Path(path) = call.func.as_ref() else {
        return None;
    };

    let last = path.path.segments.last()?.ident.to_string();

    if path.path.segments.len() == 1 {
        Some((None, last))
    } else {
        let module = path
            .path
            .segments
            .iter()
            .rev()
            .nth(1)
            .expect("path with at least two segments")
            .ident
            .to_string();
        Some((Some(module), last))
    }
}

fn resolve_call(
    current_tag: &str,
    call: &(Option<String>, String),
    table: &BTreeMap<(String, String), FnInfo>,
) -> Option<(String, String)> {
    let key = match call {
        (None, name) => (current_tag.to_owned(), name.clone()),
        (Some(module), name) => (module.clone(), name.clone()),
    };

    table.contains_key(&key).then_some(key)
}

// The ONLY currently-allowed dispatched todo!. Tracked separately on branch
// fix/get-output-distribution-panic (monero #9422: distribution type holds binary strings).
// When that fix lands this test goes RED (stale allowlist) - remove this entry then.
const ALLOWLIST: &[(&str, &str)] = &[("json_rpc", "get_output_distribution")];

fn expected(entries: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    entries
        .iter()
        .map(|(tag, handler)| ((*tag).to_owned(), (*handler).to_owned()))
        .collect()
}

#[test]
fn all_not_available_arms_are_ignored() {
    let files = [HandlerFile {
        tag: "t",
        src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Alpha(r) => Resp::Alpha(not_available()?),
        Req::Beta(r) => Resp::Beta(not_available()?),
    })
}

async fn alpha(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    todo!()
}

async fn beta(state: State, request: BetaReq) -> Result<BetaResp, Error> {
    todo!()
}
",
    }];

    assert_eq!(dispatched_todo_violations(&files), expected(&[]));
}

#[test]
fn direct_dispatched_handler_with_todo_is_reported() {
    let files = [HandlerFile {
        tag: "t",
        src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Alpha(r) => Resp::Alpha(handler(state, r).await?),
    })
}

async fn handler(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    todo!()
}
",
    }];

    assert_eq!(
        dispatched_todo_violations(&files),
        expected(&[("t", "handler")])
    );
}

#[test]
fn transitive_shared_call_with_todo_is_reported_on_dispatch_target() {
    let files = [
        HandlerFile {
            tag: "t",
            src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Alpha(r) => Resp::Alpha(wrapper(state, r).await?),
    })
}

async fn wrapper(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    shared::inner(state, request).await
}
",
        },
        HandlerFile {
            tag: "shared",
            src: r"
use anyhow::Error;

pub async fn inner(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    todo!()
}
",
        },
    ];

    assert_eq!(
        dispatched_todo_violations(&files),
        expected(&[("t", "wrapper")])
    );
}

#[test]
fn direct_shared_dispatch_with_todo_is_reported() {
    let files = [
        HandlerFile {
            tag: "t",
            src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Alpha(r) => Resp::Alpha(shared::inner(state, r).await?),
    })
}
",
        },
        HandlerFile {
            tag: "shared",
            src: r"
use anyhow::Error;

pub async fn inner(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    todo!()
}
",
        },
    ];

    assert_eq!(
        dispatched_todo_violations(&files),
        expected(&[("t", "inner")])
    );
}

#[test]
fn transitive_helper_call_with_todo_is_reported() {
    let files = [
        HandlerFile {
            tag: "t",
            src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Alpha(r) => Resp::Alpha(wrapper(state, r).await?),
    })
}

async fn wrapper(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    helper::compute(state, request)
}
",
        },
        HandlerFile {
            tag: "helper",
            src: r"
use anyhow::Error;

pub fn compute(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    todo!()
}
",
        },
    ];

    assert_eq!(
        dispatched_todo_violations(&files),
        expected(&[("t", "wrapper")])
    );
}

#[test]
fn prelude_match_before_request_dispatch_does_not_hide_handler() {
    let files = [HandlerFile {
        tag: "t",
        src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    let _ignored = match 0 {
        0 => 1,
        _ => 2,
    };

    Ok(match request {
        Req::Alpha(r) => Resp::Alpha(handler(state, r).await?),
    })
}

async fn handler(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    todo!()
}
",
    }];

    assert_eq!(
        dispatched_todo_violations(&files),
        expected(&[("t", "handler")])
    );
}

#[test]
fn todo_only_reachable_from_not_available_arm_is_ignored() {
    let files = [HandlerFile {
        tag: "t",
        src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Alpha(r) => Resp::Alpha(not_available()?),
    })
}

async fn hidden_todo(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    todo!()
}
",
    }];

    assert_eq!(dispatched_todo_violations(&files), expected(&[]));
}

#[test]
fn unsupported_return_err_arms_are_ignored_without_panicking() {
    let files = [HandlerFile {
        tag: "t",
        src: r#"
use anyhow::{anyhow, Error};

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Alpha(_) | Req::Beta(_) => return Err(anyhow!("x")),
    })
}

async fn alpha(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    todo!()
}
"#,
    }];

    assert_eq!(dispatched_todo_violations(&files), expected(&[]));
}

#[test]
fn unimplemented_is_reported_the_same_as_todo() {
    let files = [HandlerFile {
        tag: "t",
        src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Alpha(r) => Resp::Alpha(handler(state, r).await?),
    })
}

async fn handler(state: State, request: AlphaReq) -> Result<AlphaResp, Error> {
    unimplemented!()
}
",
    }];

    assert_eq!(
        dispatched_todo_violations(&files),
        expected(&[("t", "handler")])
    );
}

#[test]
fn shared_name_collision_only_flags_the_resolved_shared_call_path() {
    let files = [
        HandlerFile {
            tag: "t",
            src: r"
use anyhow::Error;

pub async fn map_request(state: State, request: Req) -> Result<Resp, Error> {
    Ok(match request {
        Req::Clean(r) => Resp::Clean(foo(state, r).await?),
        Req::Shared(r) => Resp::Shared(call_shared(state, r).await?),
    })
}

async fn foo(state: State, request: CleanReq) -> Result<CleanResp, Error> {
    Ok(CleanResp)
}

async fn call_shared(state: State, request: SharedReq) -> Result<SharedResp, Error> {
    shared::foo(state, request).await
}
",
        },
        HandlerFile {
            tag: "shared",
            src: r"
use anyhow::Error;

pub async fn foo(state: State, request: SharedReq) -> Result<SharedResp, Error> {
    todo!()
}
",
        },
    ];

    assert_eq!(
        dispatched_todo_violations(&files),
        expected(&[("t", "call_shared")])
    );
}

#[test]
fn dispatched_handlers_have_no_reachable_todo() {
    let files = [
        HandlerFile {
            tag: "json_rpc",
            src: include_str!("json_rpc.rs"),
        },
        HandlerFile {
            tag: "other_json",
            src: include_str!("other_json.rs"),
        },
        HandlerFile {
            tag: "bin",
            src: include_str!("bin.rs"),
        },
        HandlerFile {
            tag: "shared",
            src: include_str!("shared.rs"),
        },
        HandlerFile {
            tag: "helper",
            src: include_str!("helper.rs"),
        },
    ];
    let expected = expected(ALLOWLIST);
    let actual = dispatched_todo_violations(&files);

    assert_eq!(
        actual,
        expected,
        "unexpected dispatched todo!/unimplemented! set: {actual:?}; offending (file, handler) pairs must be routed to not_available() or removed from the stale allowlist entry"
    );
}
