use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query,
    },
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use base64::Engine;
use crate::context::create_context;
use crate::db::get_db;
use crate::execution::run_execution_loop;
use crate::ia::selectors::query_selector;
use crate::ia::types::*;
use crate::ia::{find_state_by_id, identify_states};
use crate::plans::login::{LoginParams, LoginPlan};
use crate::plans::logout::{LogoutParams, LogoutPlan};
use crate::sessions::manager::get_session;
use crate::tools::a11y::get_a11y_desktop;
use crate::tools::exec::ExecOptions;
use crate::tools::qr::{decode_qr_from_base64, to_data_url};
use crate::tools::screenshot::capture_screenshot;

/// Only one WebSocket login flow may be active at a time.
///
/// The execution layer also serializes GUI plans, but without an admission
/// guard concurrent login requests queue up and start one after another. That
/// can repeatedly create or observe QR login sessions after the first client
/// disconnects. Reject duplicates before they enter the execution queue.
static LOGIN_WS_LOCK: Mutex<()> = Mutex::const_new(());

fn has_logged_in_navigation_shell(a11y: &A11yNode) -> bool {
    query_selector(a11y, r#"tool-bar[name="Navigation"]"#).is_some()
        && query_selector(a11y, r#"tool-bar[name="Navigation"] push-button[name=/^(Weixin|WeChat)$/]"#).is_some()
        && query_selector(a11y, r#"tool-bar[name="Navigation"] push-button[name="Contacts"]"#).is_some()
        && query_selector(a11y, r#"tool-bar[name="Navigation"] push-button[name="More"]"#).is_some()
}

fn has_explicit_login_ui(a11y: &A11yNode) -> bool {
    query_selector(a11y, r#"label[name="Entering"]"#).is_some()
        || query_selector(a11y, r#"label[name*="Loading"]"#).is_some()
        || query_selector(a11y, r#"label[name*="Scan to log in"]"#).is_some()
        || query_selector(a11y, r#"push-button[name="Switch Account"]"#).is_some()
        || query_selector(a11y, r#"label[name=/Comfirm on phone|Confirm.*phone|手机确认/i]"#).is_some()
}

fn observed_auth_status(a11y: &A11yNode, identified: &IdentifiedStates) -> &'static str {
    if has_explicit_login_ui(a11y) {
        return "logged_out";
    }
    if let Some(main_window) = identified.main_window.as_ref() {
        return match main_window.state_id.as_str() {
            "chat" | "chat_open" => "logged_in",
            "login_qr" | "login_account" | "login_phone_confirm" | "login_loading" => "logged_out",
            _ if has_logged_in_navigation_shell(a11y) => "logged_in",
            _ => "unknown",
        };
    }
    if has_logged_in_navigation_shell(a11y) { "logged_in" } else { "unknown" }
}

pub async fn get_status() -> Json<serde_json::Value> {
    let session = get_session("default");
    let wechat_running = crate::tools::wechat_db::find_wechat_pid().is_some();
    let login_state = match &session {
        Some(s) if wechat_running && s.login_state == "logged_in" => "logged_in",
        _ => "logged_out",
    };
    let logged_in_user = if wechat_running {
        session.as_ref().and_then(|s| s.logged_in_user.clone())
    } else {
        None
    };

    Json(serde_json::json!({
        "container": "running",
        "loginState": { "status": login_state },
        "loggedInUser": logged_in_user,
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// Check auth status via one FSM observation cycle.
///
/// Explicit login views are logged out. Chat views and the complete logged-in
/// navigation shell are logged in. Other observations remain unknown.
pub async fn auth_status() -> Json<serde_json::Value> {
    let session = match get_session("default") {
        Some(s) => s,
        None => {
            return Json(serde_json::json!({
                "status": "unknown",
            }))
        }
    };

    // Check if WeChat process is running first
    let wechat_running = crate::tools::wechat_db::find_wechat_pid().is_some();
    if !wechat_running {
        return Json(serde_json::json!({
            "status": "app_not_running",
            "loggedInUser": session.logged_in_user,
        }));
    }

    let exec_options = ExecOptions {
        session: Some(session.clone()),
        timeout_ms: 30_000,
    };

    // Run one observation: a11y → identify → reduce
    let a11y = match get_a11y_desktop(&exec_options).await {
        Ok(tree) => tree,
        Err(_) => {
            return Json(serde_json::json!({
                "status": "unknown",
                "loggedInUser": session.logged_in_user,
            }))
        }
    };

    let screenshot = capture_screenshot(&exec_options)
        .await
        .unwrap_or_default();
    let identified = identify_states(&a11y, &screenshot);
    let status = observed_auth_status(&a11y, &identified);

    tracing::info!(
        "[auth_status] identified={:?}, navigation_shell={}, status={}",
        identified.main_window.as_ref().map(|s| s.state_id.as_str()),
        has_logged_in_navigation_shell(&a11y),
        status
    );

    Json(serde_json::json!({
        "status": status,
        "loggedInUser": session.logged_in_user,
    }))
}

/// Log out of WeChat via FSM execution loop.
pub async fn logout() -> Json<serde_json::Value> {
    let session = match get_session("default") {
        Some(s) => s,
        None => {
            return Json(serde_json::json!({
                "success": false,
                "error": "No session available"
            }))
        }
    };

    // Quick auth check first
    let exec_options = ExecOptions {
        session: Some(session.clone()),
        timeout_ms: 30_000,
    };

    let a11y = match get_a11y_desktop(&exec_options).await {
        Ok(tree) => tree,
        Err(e) => {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("Failed to get a11y tree: {e}")
            }))
        }
    };

    let screenshot = capture_screenshot(&exec_options).await.unwrap_or_default();
    let identified = identify_states(&a11y, &screenshot);

    // Load persisted state and check if logged in
    let mut context = {
        let db = get_db();
        create_context(session.clone(), &db)
    };

    if let Some(ref mw) = identified.main_window {
        if let Some(state_impl) = find_state_by_id(&mw.state_id) {
            let screenshot_bytes = base64::engine::general_purpose::STANDARD
                .decode(&screenshot)
                .unwrap_or_default();
            context.state = state_impl.reduce(&ReduceArgs {
                prev: &context.state,
                a11y: &a11y,
                screenshot: &screenshot_bytes,
            });
        }
    }

    if !context.state.main_window.is_logged_in {
        return Json(serde_json::json!({
            "success": false,
            "error": "Not logged in"
        }));
    }

    // Run logout FSM
    let cancel = CancellationToken::new();
    let plan = LogoutPlan;
    let params = LogoutParams;
    let emit = |_event: SubscriptionEvent| {};
    let (result, _) = run_execution_loop(&plan, &params, &mut context, &emit, cancel).await;

    if result.success {
        // Clear logged_in_user from session
        let db = get_db();
        crate::db::queries::update_session_logged_in_user(&db, &session.id, None);
    }

    Json(serde_json::json!({
        "success": result.success,
        "error": result.error
    }))
}

pub async fn login() -> Json<serde_json::Value> {
    let screenshot = capture_screenshot(&ExecOptions::default()).await;

    match screenshot {
        Ok(b64) => {
            if let Some(qr_result) = decode_qr_from_base64(&b64) {
                let data_url = to_data_url(&qr_result.data).ok();
                return Json(serde_json::json!({
                    "success": false,
                    "state": { "status": "qr_pending" },
                    "qrDataUrl": data_url
                }));
            }

            Json(serde_json::json!({
                "success": false,
                "state": { "status": "qr_pending" }
            }))
        }
        Err(_) => Json(serde_json::json!({
            "success": false,
            "state": { "status": "logged_out" }
        })),
    }
}

#[derive(Deserialize)]
pub struct LoginWsParams {
    #[serde(rename = "timeoutMs", default = "default_timeout")]
    timeout_ms: u64,
    #[serde(rename = "newAccount", default)]
    new_account: bool,
}

fn default_timeout() -> u64 {
    300_000
}

pub async fn login_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<LoginWsParams>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_login_ws(socket, params))
}

async fn handle_login_ws(mut socket: WebSocket, params: LoginWsParams) {
    let _login_guard = match LOGIN_WS_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!("[login] Rejected concurrent login WebSocket");
            let msg = serde_json::to_string(&LoginSubscriptionEvent::Error {
                message: "A login session is already in progress; reuse the active QR code"
                    .to_string(),
            })
            .unwrap();
            let _ = socket.send(Message::Text(msg.into())).await;
            return;
        }
    };

    let session = match get_session("default") {
        Some(s) => s,
        None => {
            let msg = serde_json::to_string(&LoginSubscriptionEvent::Error {
                message: "No session available".to_string(),
            })
            .unwrap();
            let _ = socket.send(Message::Text(msg.into())).await;
            return;
        }
    };

    // Send initial status
    let msg = serde_json::to_string(&LoginSubscriptionEvent::Status {
        message: "Navigating login flow...".to_string(),
    })
    .unwrap();
    if socket.send(Message::Text(msg.into())).await.is_err() {
        return;
    }

    // Channel to bridge sync emit callback → async WebSocket sends
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubscriptionEvent>();
    let cancel = CancellationToken::new();
    let cancel_for_exec = cancel.clone();
    let login_params = LoginParams {
        new_account: params.new_account,
    };

    // Spawn the execution loop in a separate task
    let exec_handle = tokio::spawn(async move {
        let mut context = {
            let db = get_db();
            create_context(session, &db)
        };
        let plan = LoginPlan;
        let emit = move |event: SubscriptionEvent| {
            let _ = tx.send(event);
        };
        run_execution_loop(&plan, &login_params, &mut context, &emit, cancel_for_exec).await.0
    });

    // Main loop: bridge events to WebSocket, handle timeout + disconnect
    let timeout = tokio::time::sleep(std::time::Duration::from_millis(params.timeout_ms));
    tokio::pin!(timeout);
    let mut sent_terminal = false;
    let mut client_disconnected = false;
    let mut server_timeout = false;

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(evt) => {
                        let ws_event = subscription_event_to_login_event(evt);
                        if is_terminal_login_event(&ws_event) {
                            sent_terminal = true;
                        }
                        let msg = serde_json::to_string(&ws_event).unwrap();
                        if socket.send(Message::Text(msg.into())).await.is_err() {
                            cancel.cancel();
                            client_disconnected = true;
                            break;
                        }
                    }
                    None => break, // channel closed = execution done
                }
            }
            _ = &mut timeout => {
                cancel.cancel();
                server_timeout = true;
                sent_terminal = true;
                let msg = serde_json::to_string(&LoginSubscriptionEvent::LoginTimeout).unwrap();
                if socket.send(Message::Text(msg.into())).await.is_err() {
                    client_disconnected = true;
                }
                break;
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(_)) => continue,
                    _ => {
                        cancel.cancel();
                        client_disconnected = true;
                        break;
                    }
                }
            }
        }
    }

    // Wait for execution to finish and emit a fallback terminal event if needed.
    let exec_result = exec_handle.await.ok();
    if !client_disconnected && !sent_terminal {
        let fallback = match exec_result {
            Some(result) if result.success => LoginSubscriptionEvent::LoginSuccess { user_id: None },
            Some(result) => {
                let message = result.error.unwrap_or_else(|| "Login failed".to_string());
                if message.starts_with("Unknown state for")
                    || message.starts_with("Execution timeout after")
                    || (server_timeout && message == "Aborted")
                {
                    LoginSubscriptionEvent::LoginTimeout
                } else {
                    LoginSubscriptionEvent::Error { message }
                }
            }
            None => LoginSubscriptionEvent::Error {
                message: "Login execution task failed".to_string(),
            },
        };
        let msg = serde_json::to_string(&fallback).unwrap();
        let _ = socket.send(Message::Text(msg.into())).await;
    }
}

fn is_terminal_login_event(event: &LoginSubscriptionEvent) -> bool {
    matches!(
        event,
        LoginSubscriptionEvent::LoginSuccess { .. }
            | LoginSubscriptionEvent::LoginTimeout
            | LoginSubscriptionEvent::Error { .. }
    )
}

/// Convert generic SubscriptionEvent (from plans) to typed LoginSubscriptionEvent (for WS).
fn subscription_event_to_login_event(event: SubscriptionEvent) -> LoginSubscriptionEvent {
    match event.event_type.as_str() {
        "status" => LoginSubscriptionEvent::Status {
            message: event
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "qr" => {
            let qr_data = event
                .data
                .get("qrData")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let qr_data_url = to_data_url(&qr_data).ok();
            LoginSubscriptionEvent::Qr {
                qr_data,
                qr_binary_data: None,
                qr_data_url,
            }
        }
        "phone_confirm" => LoginSubscriptionEvent::PhoneConfirm {
            message: event
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        },
        "login_success" => LoginSubscriptionEvent::LoginSuccess {
            user_id: event
                .data
                .get("userId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        },
        "login_timeout" => LoginSubscriptionEvent::LoginTimeout,
        "error" => LoginSubscriptionEvent::Error {
            message: event
                .data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown error")
                .to_string(),
        },
        _ => LoginSubscriptionEvent::Status {
            message: format!("Unknown event: {}", event.event_type),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{observed_auth_status, LOGIN_WS_LOCK};
    use crate::ia::identify_states;
    use crate::ia::types::A11yNode;

    fn node(role: &str, name: &str, children: Vec<A11yNode>) -> A11yNode {
        A11yNode {
            role: role.to_string(), name: name.to_string(), bounds: None,
            children: (!children.is_empty()).then_some(children), parent_index: None,
            window: None, states: None,
        }
    }

    fn logged_in_page(list_name: &str) -> A11yNode {
        node("desktop-frame", "main", vec![node("frame", "WeChat", vec![
            node("tool-bar", "Navigation", vec![
                node("push-button", "Weixin", vec![]),
                node("push-button", "Contacts", vec![]),
                node("push-button", "More", vec![]),
            ]),
            node("list", list_name, vec![]),
        ])])
    }

    #[tokio::test]
    async fn login_websocket_lock_allows_only_one_active_session() {
        let first = LOGIN_WS_LOCK
            .try_lock()
            .expect("first login session should acquire the lock");

        assert!(
            LOGIN_WS_LOCK.try_lock().is_err(),
            "a concurrent login session must be rejected"
        );

        drop(first);

        assert!(
            LOGIN_WS_LOCK.try_lock().is_ok(),
            "the lock must be released after the active login session ends"
        );
    }

    #[test]
    fn service_accounts_page_is_logged_in_without_becoming_a_chat_state() {
        let a11y = logged_in_page("Service Accounts");
        let identified = identify_states(&a11y, "");
        assert!(identified.main_window.is_none());
        assert_eq!(observed_auth_status(&a11y, &identified), "logged_in");
    }

    #[test]
    fn explicit_login_loading_overrides_navigation_shell() {
        let mut a11y = logged_in_page("Service Accounts");
        a11y.children.as_mut().unwrap()[0].children.as_mut().unwrap()
            .push(node("label", "Entering", vec![]));
        let identified = identify_states(&a11y, "");
        assert_eq!(identified.main_window.as_ref().map(|s| s.state_id.as_str()), Some("login_loading"));
        assert_eq!(observed_auth_status(&a11y, &identified), "logged_out");
    }

    #[test]
    fn partial_navigation_is_unknown() {
        let a11y = node("desktop-frame", "main", vec![node("tool-bar", "Navigation", vec![
            node("push-button", "Weixin", vec![]),
            node("push-button", "Contacts", vec![]),
        ])]);
        let identified = identify_states(&a11y, "");
        assert_eq!(observed_auth_status(&a11y, &identified), "unknown");
    }

    #[test]
    fn qr_login_elements_are_logged_out_even_without_qr_decode() {
        let a11y = node("desktop-frame", "main", vec![node("frame", "WeChat", vec![
            node("label", "Scan to log in", vec![]),
            node("push-button", "Transfer files only", vec![]),
        ])]);
        let identified = identify_states(&a11y, "");
        assert!(identified.main_window.is_none());
        assert_eq!(observed_auth_status(&a11y, &identified), "logged_out");
    }
}
