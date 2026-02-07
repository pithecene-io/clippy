//! CLI client for broker operations.
//!
//! Provides one-shot commands that connect to the broker, perform a
//! single request, print the result, and exit. Covers all v0 and v1
//! broker operations: session queries, capture/paste, turn registry
//! lookups, and sink delivery.

mod broker_client;
mod format;

use crate::cli::ClientAction;
use broker_client::BrokerClient;

/// Client error type.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("broker: {0}")]
    Broker(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run the client command.
///
/// Connects to the broker, performs the requested action, prints the
/// result, and returns. Called from `main.rs` for `Command::Client`.
pub async fn run(action: ClientAction) -> Result<(), ClientError> {
    let mut broker = BrokerClient::connect().await?;

    match action {
        ClientAction::ListSessions => {
            let sessions = broker.list_sessions().await?;
            format::print_sessions(&sessions);
        }
        ClientAction::ListTurns { session, limit } => {
            let turns = broker.list_turns(&session, limit).await?;
            format::print_turns(&turns);
        }
        ClientAction::GetTurn {
            turn_id,
            metadata_only,
        } => {
            let result = broker.get_turn(&turn_id).await?;
            format::print_turn(&turn_id, &result, metadata_only)?;
        }
        ClientAction::Capture { session } => {
            let result = broker.capture(&session).await?;
            format::print_capture(&result);
        }
        ClientAction::CaptureByID { turn_id } => {
            let result = broker.capture_by_id(&turn_id).await?;
            format::print_capture(&result);
        }
        ClientAction::Paste { session } => {
            broker.paste(&session).await?;
            format::print_paste(&session);
        }
        ClientAction::Deliver {
            sink,
            session,
            path,
        } => {
            validate_deliver_args(&sink, &session, &path)?;
            broker.deliver(&sink, session, path).await?;
            format::print_deliver(&sink);
        }
    }

    Ok(())
}

/// Validate deliver arguments before sending to the broker.
///
/// Checks cross-field constraints: inject requires `--session`, file
/// requires `--path`. Unknown sink names are rejected.
fn validate_deliver_args(
    sink: &str,
    session: &Option<String>,
    path: &Option<String>,
) -> Result<(), ClientError> {
    match sink {
        "clipboard" => Ok(()),
        "inject" => {
            if session.is_none() {
                Err(ClientError::Broker(
                    "--session is required for inject sink".into(),
                ))
            } else {
                Ok(())
            }
        }
        "file" => {
            if path.is_none() {
                Err(ClientError::Broker(
                    "--path is required for file sink".into(),
                ))
            } else {
                Ok(())
            }
        }
        other => Err(ClientError::Broker(format!(
            "unknown sink: {other} (expected: clipboard, file, inject)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_deliver_clipboard_ok() {
        assert!(validate_deliver_args("clipboard", &None, &None).is_ok());
    }

    #[test]
    fn validate_deliver_inject_ok() {
        assert!(validate_deliver_args("inject", &Some("s1".into()), &None).is_ok());
    }

    #[test]
    fn validate_deliver_inject_missing_session() {
        let err = validate_deliver_args("inject", &None, &None).unwrap_err();
        assert!(err.to_string().contains("--session"));
    }

    #[test]
    fn validate_deliver_file_ok() {
        assert!(validate_deliver_args("file", &None, &Some("/tmp/out".into())).is_ok());
    }

    #[test]
    fn validate_deliver_file_missing_path() {
        let err = validate_deliver_args("file", &None, &None).unwrap_err();
        assert!(err.to_string().contains("--path"));
    }

    #[test]
    fn validate_deliver_unknown_sink() {
        let err = validate_deliver_args("foobar", &None, &None).unwrap_err();
        assert!(err.to_string().contains("unknown sink"));
    }
}
