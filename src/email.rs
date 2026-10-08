use lettre::transport::smtp;

use crate::metrics;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Config {
    smtp_url: String,
    from: lettre::message::Mailbox,
    to: lettre::message::Mailbox,
}

/// An email to send.
pub struct Email {
    pub subject: String,
    pub body: String,
    pub message_id: String,
    /// Message ID of the email this replies to, so that mail clients thread them together.
    pub in_reply_to: Option<String>,
}

pub trait Notifier: Send + Sync {
    fn notify(&self, email: &Email);
}

pub struct NoOpNotifier;

impl Notifier for NoOpNotifier {
    fn notify(&self, email: &Email) {
        eprintln!(
            "[email] notifications disabled; skipping sending notification with subject {}:\n{}",
            email.subject, email.body
        );
    }
}

pub struct Client {
    config: Config,
}

impl Client {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl Notifier for Client {
    fn notify(&self, email: &Email) {
        use lettre::message::header::ContentType;
        use lettre::Message;
        use lettre::Transport;
        eprintln!("[email] sending email with subject {}", email.subject);
        let transport = smtp::SmtpTransport::from_url(&self.config.smtp_url)
            .unwrap()
            .build();
        match transport.test_connection() {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("[email] failed to connect to SMTP server");
            }
            Err(err) => {
                eprintln!("[email] failed to connect to SMTP server: {err:?}");
            }
        }
        let mut builder = Message::builder()
            .from(self.config.from.clone())
            .to(self.config.to.clone())
            .subject(&email.subject)
            .message_id(Some(email.message_id.clone()))
            .header(ContentType::TEXT_PLAIN);
        if let Some(parent) = &email.in_reply_to {
            builder = builder
                .in_reply_to(parent.clone())
                .references(parent.clone());
        }
        let message = builder.body(email.body.clone()).unwrap();
        let result = match transport.send(&message) {
            Ok(_) => {
                eprintln!("Email sent successfully!");
                "success"
            }
            Err(err) => {
                eprintln!("Failed to send email: {err:?}");
                "failure"
            }
        };
        metrics::get()
            .notifications
            .with_label_values(&[result])
            .inc();
    }
}
