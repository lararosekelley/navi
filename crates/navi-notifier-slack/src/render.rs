//! Pure rendering of a navi [`Event`] into a Slack message (fallback text + Block
//! Kit blocks). Kept free of I/O so message shape is unit-testable.

// Slack mrkdwn escapes the same three characters as HTML (`&`, `<`, `>`), so the
// core `html_escape` is reused verbatim rather than duplicated here.
use navi_notifier_core::html_escape;
use navi_notifier_core::model::{Event, EventKind, MergeQueueRemoval, ReviewState};
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
    let context_bits = [
        format!("<{}|{}>", pr.url, repo_ref),
        format!("by {}", event.pull_request.author.label()),
    ];

    let mut blocks = vec![json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": format!("{}\n<{}|{}: {}>", headline, link_url, repo_ref, html_escape(&pr.title)),
        }
    })];

    if let Some(excerpt) = &event.excerpt {
        blocks.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": format!("> {}", html_escape(excerpt)) }
        }));
    }

    blocks.push(json!({
        "type": "context",
        "elements": [ { "type": "mrkdwn", "text": context_bits.join("  ·  ") } ]
    }));

    Rendered { text, blocks }
}

/// Render a batch of events as a single digest message: a header plus one line
/// per event. Assumes `events` is non-empty.
pub fn render_digest(events: &[Event]) -> Rendered {
    let n = events.len();
    let header = format!(
        ":inbox_tray: *navi digest* · {n} update{}",
        if n == 1 { "" } else { "s" }
    );
    let lines: Vec<String> = events
        .iter()
        .map(|e| {
            let pr = &e.pull_request;
            let repo_ref = format!("{}#{}", pr.repo.full_name(), pr.number);
            format!(
                "{}  ·  <{}|{}>",
                headline(e, e.actor_label()),
                e.target_url.clone().unwrap_or_else(|| pr.url.clone()),
                repo_ref
            )
        })
        .collect();
    Rendered {
        text: format!("navi digest: {n} update{}", if n == 1 { "" } else { "s" }),
        blocks: vec![json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": format!("{header}\n{}", lines.join("\n")) }
        })],
    }
}

/// The one-line headline with a leading emoji, in Slack mrkdwn.
fn headline(event: &Event, actor: &str) -> String {
    // Bold, escaped actor + the shared English sentence (pr phrase escaped for mrkdwn).
    let bold = format!("*{}*", html_escape(actor));
    format!(
        "{} {}",
        emoji(&event.kind),
        event.headline(&bold, html_escape)
    )
}

/// The leading emoji for each event kind (Slack-specific; the wording is shared).
fn emoji(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::ReviewRequested => ":eyes:",
        EventKind::ReReviewRequested => ":arrows_counterclockwise:",
        EventKind::ReviewSubmitted { state } => match state {
            ReviewState::Approved => ":white_check_mark:",
            ReviewState::ChangesRequested => ":warning:",
            ReviewState::Commented => ":speech_balloon:",
        },
        EventKind::ReviewDismissed => ":recycle:",
        EventKind::CommentReply { .. } => ":left_speech_bubble:",
        EventKind::Mentioned => ":wave:",
        EventKind::Merged => ":purple_heart:",
        EventKind::Closed => ":no_entry_sign:",
        EventKind::ReadyForReview => ":rocket:",
        EventKind::EnteredMergeQueue => ":train:",
        EventKind::RemovedFromMergeQueue { reason } => match reason {
            MergeQueueRemoval::Dequeued => ":arrow_backward:",
            MergeQueueRemoval::Unmergeable => ":warning:",
        },
    }
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

    #[test]
    fn digest_has_a_header_and_one_line_per_event() {
        let events = [event(EventKind::Merged), event(EventKind::Mentioned)];
        let r = render_digest(&events);
        assert_eq!(r.text, "navi digest: 2 updates");
        let body = r.blocks[0]["text"]["text"].as_str().unwrap();
        assert!(body.contains("navi digest* · 2 updates"));
        assert!(body.contains("merged octo's PR"));
        assert!(body.contains("mentioned you"));
    }
}
