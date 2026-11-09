use crate::config::{ObservabilityConfig, SmtpConfig};

// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum AlertLevel {
    Warning,
    Error,
}

impl AlertLevel {
    fn label(&self) -> &'static str {
        match self {
            AlertLevel::Warning => "WARNING",
            AlertLevel::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AlertPayload {
    pub job_id: String,
    pub message: String,
    pub level: AlertLevel,
}

// ---------------------------------------------------------------------------

/// Fire webhook and/or email. Errors are logged but never propagated.
pub async fn notify(
    payload: AlertPayload,
    config: &ObservabilityConfig,
    smtp: &Option<SmtpConfig>,
) {
    if let Some(url) = &config.webhook_url {
        send_webhook(url, &payload).await;
    }
    if let (Some(to), Some(smtp_cfg)) = (&config.alert_email, smtp) {
        send_email(to, smtp_cfg, &payload).await;
    }
}

// ---------------------------------------------------------------------------
// Webhook (ureq, sync → spawn_blocking)
// ---------------------------------------------------------------------------

async fn send_webhook(url: &str, payload: &AlertPayload) {
    let url = url.to_string();
    let body = format!(
        r#"{{"text":"[rar2zip] {}: {}","job_id":"{}"}}"#,
        payload.level.label(),
        payload.message.replace('"', "'"),
        payload.job_id,
    );

    let result = tokio::task::spawn_blocking(move || {
        ureq::post(&url)
            .timeout(std::time::Duration::from_secs(5))
            .set("Content-Type", "application/json")
            .send_string(&body)
            .map_err(Box::new)
    })
    .await;

    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("webhook delivery failed: {e}"),
        Err(e) => tracing::warn!("webhook task panicked: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Email (lettre async SMTP)
// ---------------------------------------------------------------------------

async fn send_email(to: &str, cfg: &SmtpConfig, payload: &AlertPayload) {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let from_mb = match cfg.from.parse() {
        Ok(mb) => mb,
        Err(e) => {
            tracing::warn!("alert email: invalid from address '{}': {e}", cfg.from);
            return;
        }
    };
    let to_mb = match to.parse() {
        Ok(mb) => mb,
        Err(e) => {
            tracing::warn!("alert email: invalid to address '{to}': {e}");
            return;
        }
    };

    let subject = format!(
        "[rar2zip] {}: job {}",
        payload.level.label(),
        payload.job_id,
    );
    let msg = match Message::builder()
        .from(from_mb)
        .to(to_mb)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(payload.message.clone())
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("alert email: message build failed: {e}");
            return;
        }
    };

    let creds = Credentials::new(cfg.user.clone(), cfg.password.clone());
    let mailer = match AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host) {
        Ok(b) => b.port(cfg.port).credentials(creds).build(),
        Err(e) => {
            tracing::warn!("alert email: SMTP transport build failed: {e}");
            return;
        }
    };

    if let Err(e) = mailer.send(msg).await {
        tracing::warn!("alert email: send failed: {e}");
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ObservabilityConfig;

    #[test]
    fn alert_level_label() {
        assert_eq!(AlertLevel::Warning.label(), "WARNING");
        assert_eq!(AlertLevel::Error.label(), "ERROR");
    }

    #[tokio::test]
    async fn notify_with_no_webhook_or_email_does_not_panic() {
        let cfg = ObservabilityConfig {
            webhook_url: None,
            alert_email: None,
            ..Default::default()
        };
        let payload = AlertPayload {
            job_id: "test-job".into(),
            message: "test message".into(),
            level: AlertLevel::Warning,
        };
        notify(payload, &cfg, &None).await; // must complete without panic
    }
}
