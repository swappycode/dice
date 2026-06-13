//! Outbound transactional mail (email-verify, password-reset).
//!
//! [`Mailer`] is the seam; [`LogMailer`] is the dev default — it logs the
//! message (verification/reset token included) at INFO instead of talking SMTP,
//! so the whole flow is exercisable with no mail server. A real SMTP/API impl
//! drops in behind the trait later (mirrors media-service's `MediaStore` seam),
//! and `AuthService::with_mailer` swaps it in.

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("mail transport: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// One outbound message. Plain-text only — these are short transactional mails.
#[derive(Debug, Clone)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub body: String,
}

#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, mail: Mail) -> Result<(), MailError>;
}

/// Dev/test mailer: logs the message (so the token is visible in the server log)
/// and never fails. NEVER use in production — tokens would leak to logs.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogMailer;

#[async_trait]
impl Mailer for LogMailer {
    async fn send(&self, mail: Mail) -> Result<(), MailError> {
        tracing::info!(
            to = %mail.to,
            subject = %mail.subject,
            "LogMailer: would send mail\n{}",
            mail.body
        );
        Ok(())
    }
}
