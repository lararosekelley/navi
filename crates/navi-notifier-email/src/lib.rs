//! Email destination for navi.
//!
//! Delivers each event as an SMTP message. All events for one PR share an
//! `In-Reply-To`/`References` id keyed by `{source}:{repo}#{number}`, so a mail
//! client threads them together; each message gets its own unique `Message-ID`.

use async_trait::async_trait;
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use navi_notifier_core::model::{Event, EventKind, ReviewState};
use navi_notifier_core::traits::Destination;
use navi_notifier_core::DestinationError;
use tracing::debug;

/// How to secure the SMTP connection.
#[derive(Debug, Clone, Copy)]
pub enum EmailTls {
    /// Unencrypted (e.g. a local Mailpit sink). Do not use over a network.
    None,
    /// STARTTLS upgrade, typically port 587.
    StartTls,
    /// Implicit TLS, typically port 465.
    Implicit,
}

pub struct EmailDestinationConfig {
    pub smtp_host: String,
    pub smtp_port: u16,
    pub tls: EmailTls,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Sender, e.g. `navi <navi@example.com>`.
    pub from: String,
    /// Recipient, e.g. `you <you@example.com>`.
    pub to: String,
}

pub struct EmailDestination {
    mailer: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    to: Mailbox,
}

impl EmailDestination {
    pub fn new(config: EmailDestinationConfig) -> Result<Self, DestinationError> {
        let from = config
            .from
            .parse::<Mailbox>()
            .map_err(|e| DestinationError::Delivery(format!("invalid `from` address: {e}")))?;
        let to = config
            .to
            .parse::<Mailbox>()
            .map_err(|e| DestinationError::Delivery(format!("invalid `to` address: {e}")))?;

        let mut builder = match config.tls {
            EmailTls::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.smtp_host)
            }
            EmailTls::StartTls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
                    .map_err(|e| DestinationError::Delivery(format!("smtp starttls setup: {e}")))?
            }
            EmailTls::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_host)
                .map_err(|e| DestinationError::Delivery(format!("smtp tls setup: {e}")))?,
        }
        .port(config.smtp_port);

        if let (Some(user), Some(pass)) = (config.username, config.password) {
            builder = builder.credentials(Credentials::new(user, pass));
        }

        Ok(Self {
            mailer: builder.build(),
            from,
            to,
        })
    }

    /// Check that the SMTP server is reachable (for `test-*` style checks).
    pub async fn verify(&self) -> Result<String, DestinationError> {
        self.mailer
            .test_connection()
            .await
            .map_err(|e| DestinationError::Delivery(format!("smtp connection: {e}")))?;
        Ok(format!("smtp reachable, sending as {}", self.from))
    }
}

#[async_trait]
impl Destination for EmailDestination {
    fn id(&self) -> &str {
        "email"
    }

    async fn send(&self, event: &Event) -> Result<(), DestinationError> {
        let message = build_message(&self.from, &self.to, event)?;
        self.mailer
            .send(message)
            .await
            .map_err(|e| DestinationError::Delivery(format!("smtp send: {e}")))?;
        debug!(dedup_key = %event.dedup_key, "delivered email");
        Ok(())
    }
}

/// Build the SMTP message for an event. Pure, so it is unit-testable via
/// [`Message::formatted`].
fn build_message(from: &Mailbox, to: &Mailbox, event: &Event) -> Result<Message, DestinationError> {
    let pr = &event.pull_request;
    let repo_ref = format!("{}#{}", pr.repo.full_name(), pr.number);
    let headline = headline(event);
    let subject = format!("[{repo_ref}] {headline}");

    // Stable per-PR id so a client threads every event for one PR together.
    let thread = sanitize(&format!("navi-{}-{}", event.source_id, repo_ref));
    let thread_ref = format!("<{thread}@navi.local>");
    let own_id = format!("<{thread}-{}@navi.local>", sanitize(&event.dedup_key));

    let link = event.target_url.clone().unwrap_or_else(|| pr.url.clone());
    let text = body_text(event, &headline, &repo_ref, &link);
    let html = body_html(event, &headline, &repo_ref, &link);

    Message::builder()
        .from(from.clone())
        .to(to.clone())
        .subject(subject)
        .message_id(Some(own_id))
        .in_reply_to(thread_ref.clone())
        .references(thread_ref)
        .multipart(MultiPart::alternative_plain_html(text, html))
        .map_err(|e| DestinationError::Delivery(format!("building email: {e}")))
}

/// A plain-text, one-line headline (no markup).
fn headline(event: &Event) -> String {
    let actor = event.actor_label();
    match &event.kind {
        EventKind::ReviewRequested => format!("{actor} requested your review"),
        EventKind::ReReviewRequested => format!("{actor} requested a re-review"),
        EventKind::ReviewSubmitted { state } => match state {
            ReviewState::Approved => format!("{actor} approved your PR"),
            ReviewState::ChangesRequested => format!("{actor} requested changes"),
            ReviewState::Commented => format!("{actor} left a review comment"),
        },
        EventKind::ReviewDismissed => format!("{actor} dismissed your review"),
        EventKind::CommentReply { on_your_comment } => {
            if *on_your_comment {
                format!("{actor} replied to your comment")
            } else {
                format!("{actor} replied in a thread you're in")
            }
        }
        EventKind::Mentioned => format!("{actor} mentioned you"),
        EventKind::Merged => format!("{actor} merged your PR"),
        EventKind::Closed => "Your PR was closed".to_string(),
        EventKind::ReadyForReview => format!("{actor} marked a PR ready for review"),
    }
}

fn body_text(event: &Event, headline: &str, repo_ref: &str, link: &str) -> String {
    let mut out = format!(
        "{headline}\n\n{repo_ref}: {}\n{link}\n",
        event.pull_request.title
    );
    if let Some(excerpt) = &event.excerpt {
        out.push_str(&format!("\n> {excerpt}\n"));
    }
    out.push_str("\n-- \nsent by navi\n");
    out
}

fn body_html(event: &Event, headline: &str, repo_ref: &str, link: &str) -> String {
    let title = escape(&event.pull_request.title);
    let mut out = format!(
        "<p><strong>{}</strong></p>\n<p><a href=\"{}\">{} {}</a></p>\n",
        escape(headline),
        escape(link),
        escape(repo_ref),
        title
    );
    if let Some(excerpt) = &event.excerpt {
        out.push_str(&format!("<blockquote>{}</blockquote>\n", escape(excerpt)));
    }
    out.push_str("<p style=\"color:#888\">sent by navi</p>\n");
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Reduce a string to characters safe inside a message-id local part.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_notifier_core::model::{Actor, PullRequest, Repo, ViewerRelationship};
    use time::OffsetDateTime;

    fn event(kind: EventKind, dedup: &str) -> Event {
        Event {
            source_id: "github".into(),
            kind,
            pull_request: PullRequest {
                repo: Repo::new("acme", "widgets"),
                number: 12,
                title: "Add gizmo".into(),
                url: "https://gh.test/acme/widgets/pull/12".into(),
                author: Actor::new("octo"),
                draft: false,
            },
            viewer: ViewerRelationship::default(),
            actor: Actor::new("reviewer"),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            target_url: None,
            excerpt: Some("looks good".into()),
            dedup_key: dedup.into(),
        }
    }

    fn formatted(kind: EventKind, dedup: &str) -> String {
        let from = "navi <navi@example.com>".parse::<Mailbox>().unwrap();
        let to = "you <you@example.com>".parse::<Mailbox>().unwrap();
        let msg = build_message(&from, &to, &event(kind, dedup)).unwrap();
        String::from_utf8(msg.formatted()).unwrap()
    }

    #[test]
    fn subject_and_headline() {
        let raw = formatted(EventKind::ReviewRequested, "k1");
        assert!(raw.contains("Subject: [acme/widgets#12] reviewer requested your review"));
    }

    #[test]
    fn threads_events_for_the_same_pr() {
        // Two different events on the same PR share the thread id but have distinct Message-IDs.
        let a = formatted(EventKind::ReviewRequested, "k1");
        let b = formatted(EventKind::Merged, "k2");
        let thread = "navi-github-acme-widgets-12@navi.local";
        assert!(a.contains(&format!("References: <{thread}>")));
        assert!(b.contains(&format!("References: <{thread}>")));
        assert!(a.contains("In-Reply-To: <navi-github-acme-widgets-12@navi.local>"));
        assert!(!a.contains("Message-ID: <navi-github-acme-widgets-12@navi.local>"));
        // own id is distinct
    }

    #[test]
    fn includes_excerpt_in_body() {
        let raw = formatted(EventKind::Mentioned, "k3");
        assert!(raw.contains("looks good"));
    }
}
