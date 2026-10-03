use lettre::transport::smtp;

use crate::metrics;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Config {
    smtp_url: String,
    from: lettre::message::Mailbox,
    to: lettre::message::Mailbox,
}

pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str);
}

pub struct NoOpNotifier;

impl Notifier for NoOpNotifier {
    fn notify(&self, title: &str, _body: &str) {
        eprintln!(
            "[email] notifications disabled; skipping sending notification with title {title}"
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
    fn notify(&self, title: &str, body: &str) {
        use lettre::message::header::ContentType;
        use lettre::Message;
        use lettre::Transport;
        eprintln!("[email] sending email with title {title}");
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
        let email = Message::builder()
            .from(self.config.from.clone())
            .to(self.config.to.clone())
            .subject(title)
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string())
            .unwrap();
        let result = match transport.send(&email) {
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
