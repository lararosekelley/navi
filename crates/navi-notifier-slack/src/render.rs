//! Pure rendering of a navi [`Event`] into a Slack message (fallback text + Block
//! Kit blocks). Kept free of I/O so message shape is unit-testable.

use navi_notifier_core::model::{Event, EventKind, ReviewState};
use serde_json::{json, Value};

/// A rendered message: `text` is the notification/fallback string, `blocks` is the
/// Block Kit payload.
pub struct Rendered {
    pub text: String,
    pub blocks: Vec<Value>,
}

/// Turn an event into a Slack message.
pub fn render(event: &Event) -> Rendered {
    let pr = &event.pull_request;
    let repo_ref = format!("{}#{}", pr.repo.full_name(), pr.number);
    let actor = event.actor_label();
    let headline = headline(event, actor);

    // Fallback text (also what shows in the notification/push).
    let text = format!("{} · {}: {}", strip_mrkdwn(&headline), repo_ref, pr.title);

    let link_url = event.target_url.clone().unwrap_or_else(|| pr.url.clone());
    let mut context_bits = vec![format!("<{}|{}>", pr.url, repo_ref)];
    context_bits.push(format!("by {}", event.pull_request.author.label()));

    let mut blocks = vec![json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": format!("{}\n<{}|{}: {}>", headline, link_url, repo_ref, escape(&pr.title)),
        }
    })];

    if let Some(excerpt) = &event.excerpt {
        blocks.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": format!("> {}", escape(excerpt)) }
        }));
    }

    blocks.push(json!({
        "type": "context",
        "elements": [ { "type": "mrkdwn", "text": context_bits.join("  ·  ") } ]
    }));

    Rendered { text, blocks }
}

/// The one-line headline with a leading emoji, in Slack mrkdwn.
fn headline(event: &Event, actor: &str) -> String {
    let a = format!("*{}*", escape(actor));
    match &event.kind {
        EventKind::ReviewRequested => format!(":eyes: {a} requested your review"),
        EventKind::ReReviewRequested => {
            format!(":arrows_counterclockwise: {a} requested a re-review")
        }
        EventKind::ReviewSubmitted { state } => match state {
            ReviewState::Approved => {
                format!(
                    ":white_check_mark: {a} approved {}",
                    escape(&event.pr_phrase())
                )
            }
            ReviewState::ChangesRequested => format!(":warning: {a} requested changes"),
            ReviewState::Commented => format!(":speech_balloon: {a} left a review comment"),
        },
        EventKind::ReviewDismissed => format!(":recycle: {a} dismissed your review"),
        EventKind::CommentReply { on_your_comment } => {
            if *on_your_comment {
                format!(":left_speech_bubble: {a} replied to your comment")
            } else {
                format!(":left_speech_bubble: {a} replied in a thread you're in")
            }
        }
        EventKind::Mentioned => format!(":wave: {a} mentioned you"),
        EventKind::Merged => format!(":purple_heart: {a} merged {}", escape(&event.pr_phrase())),
        EventKind::Closed => format!(":no_entry_sign: {} was closed", escape(&event.pr_phrase())),
        EventKind::ReadyForReview => format!(":rocket: {a} marked a PR ready for review"),
    }
}

/// Escape the three characters Slack mrkdwn treats specially.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Strip `*` used for bold when building plain fallback text.
fn strip_mrkdwn(s: &str) -> String {
    s.replace('*', "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use navi_notifier_core::model::{Actor, PullRequest, Repo, ViewerRelationship};
    use time::OffsetDateTime;

    fn event(kind: EventKind) -> Event {
        Event {
            source_id: "github".into(),
            kind,
            pull_request: PullRequest {
                repo: Repo::new("acme", "widgets"),
                number: 12,
                title: "Add <gizmo> & sprocket".into(),
                url: "https://gh.test/acme/widgets/pull/12".into(),
                author: Actor::new("octo"),
                draft: false,
            },
            viewer: ViewerRelationship::default(),
            actor: Actor::new("reviewer"),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            target_url: Some("https://gh.test/rc/1".into()),
            excerpt: Some("looks good, one nit".into()),
            dedup_key: "k".into(),
        }
    }

    #[test]
    fn renders_headline_and_blocks() {
        let r = render(&event(EventKind::ReviewRequested));
        assert!(r
            .text
            .starts_with(":eyes: reviewer requested your review · acme/widgets#12:"));
        // section + excerpt + context.
        assert_eq!(r.blocks.len(), 3);
    }

    #[test]
    fn escapes_special_chars_in_title() {
        let r = render(&event(EventKind::Merged));
        let s = serde_json::to_string(&r.blocks).unwrap();
        assert!(s.contains("Add &lt;gizmo&gt; &amp; sprocket"));
        assert!(!s.contains("Add <gizmo>"));
    }

    #[test]
    fn self_action_reads_as_you() {
        let mut e = event(EventKind::Merged);
        e.viewer.actor_is_viewer = true;
        e.viewer.is_author = true;
        let r = render(&e);
        assert!(r.text.contains("you merged your PR"), "got {:?}", r.text);
        assert!(!r.text.contains("reviewer merged"));
    }

    #[test]
    fn possessive_reflects_authorship() {
        // You authored it → "your PR".
        let mut mine = event(EventKind::Merged);
        mine.viewer.is_author = true;
        assert!(render(&mine).text.contains("merged your PR"));

        // You only review it → the author's name, never "your PR".
        let theirs = render(&event(EventKind::Merged)).text; // default is_author = false
        assert!(theirs.contains("merged octo's PR"), "got {theirs}");
        assert!(!theirs.contains("your PR"));

        // The author acted on their own PR → "their own PR", not "octo merged octo's PR".
        let mut own = event(EventKind::Merged);
        own.actor = Actor::new("octo"); // same as the PR author
        let own = render(&own).text;
        assert!(own.contains("merged their own PR"), "got {own}");
        assert!(!own.contains("octo's PR"));
    }

    #[test]
    fn omits_excerpt_block_when_absent() {
        let mut e = event(EventKind::Closed);
        e.excerpt = None;
        let r = render(&e);
        assert_eq!(r.blocks.len(), 2); // section + context
    }
}
