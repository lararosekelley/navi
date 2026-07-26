//! Render a navi Event into a Discord message (content + one embed).

use navi_notifier_core::model::{Event, EventKind, MergeQueueRemoval, ReviewState};
use serde_json::{json, Value};

pub struct Rendered {
    pub content: String,
    pub embed: Value,
}

/// Turn an event into a Discord message payload.
pub fn render(event: &Event) -> Rendered {
    let pr = &event.pull_request;
    let repo_ref = format!("{}#{}", pr.repo.full_name(), pr.number);
    let actor = event.actor_label();
    let (headline, color) = headline(event, actor);
    let link = event.target_url.clone().unwrap_or_else(|| pr.url.clone());

    let mut fields = vec![
        json!({ "name": "Repo", "value": pr.repo.full_name(), "inline": true }),
        json!({ "name": "By", "value": pr.author.label(), "inline": true }),
    ];
    if let Some(excerpt) = &event.excerpt {
        fields.push(json!({ "name": "Comment", "value": truncate(excerpt, 1000) }));
    }

    let embed = json!({
        "title": truncate(&format!("{}: {}", repo_ref, pr.title), 256),
        "url": link,
        "description": headline,
        "color": color,
        "fields": fields,
        "footer": { "text": "navi" },
    });

    Rendered {
        content: format!("{}: {}", repo_ref, pr.title),
        embed,
    }
}

/// Render a batch of events as a single digest embed. Assumes `events` is non-empty.
pub fn render_digest(events: &[Event]) -> Rendered {
    let n = events.len();
    let plural = if n == 1 { "" } else { "s" };
    let lines: Vec<String> = events
        .iter()
        .map(|e| {
            let pr = &e.pull_request;
            let (h, _) = headline(e, e.actor_label());
            format!("{h} — {}#{}", pr.repo.full_name(), pr.number)
        })
        .collect();
    let embed = json!({
        "title": format!("navi digest — {n} update{plural}"),
        "description": truncate(&lines.join("\n"), 4000),
        "color": 0x5865f2u32,
        "footer": { "text": "navi" },
    });
    Rendered {
        content: format!("navi digest: {n} update{plural}"),
        embed,
    }
}

/// One-line headline plus an embed color for the event kind.
fn headline(event: &Event, actor: &str) -> (String, u32) {
    // Discord doesn't need markup-escaping, so the phrase passes through unchanged.
    let (emoji, color) = decoration(&event.kind);
    let text = format!(
        "{emoji} {}",
        event.headline(&format!("**{actor}**"), |s| s.to_string())
    );
    (text, color)
}

/// The leading emoji and embed colour for each event kind (Discord-specific; the
/// wording is shared via `Event::headline`).
fn decoration(kind: &EventKind) -> (&'static str, u32) {
    match kind {
        EventKind::ReviewRequested => ("👀", 0x5865f2),
        EventKind::ReReviewRequested => ("🔁", 0x5865f2),
        EventKind::ReviewSubmitted { state } => match state {
            ReviewState::Approved => ("✅", 0x2ecc71),
            ReviewState::ChangesRequested => ("⚠️", 0xe67e22),
            ReviewState::Commented => ("💬", 0x3498db),
        },
        EventKind::ReviewDismissed => ("♻️", 0x95a5a6),
        EventKind::CommentReply { .. } => ("💬", 0x3498db),
        EventKind::Mentioned => ("👋", 0xf1c40f),
        EventKind::Merged => ("🟣", 0x9b59b6),
        EventKind::Closed => ("🚫", 0xe74c3c),
        EventKind::ReadyForReview => ("🚀", 0x2ecc71),
        EventKind::EnteredMergeQueue => ("🚆", 0x3498db),
        EventKind::RemovedFromMergeQueue { reason } => match reason {
            MergeQueueRemoval::Dequeued => ("◀️", 0x95a5a6),
            MergeQueueRemoval::Unmergeable => ("⚠️", 0xe67e22),
        },
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    } else {
        s.to_string()
    }
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
                title: "Add gizmo".into(),
                url: "https://gh.test/acme/widgets/pull/12".into(),
                author: Actor::new("octo"),
                draft: false,
            },
            viewer: ViewerRelationship::default(),
            actor: Actor::new("reviewer"),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            target_url: Some("https://gh.test/rc/1".into()),
            excerpt: Some("looks good".into()),
            dedup_key: "k".into(),
        }
    }

    #[test]
    fn renders_embed_with_headline_and_color() {
        let r = render(&event(EventKind::ReviewRequested));
        assert_eq!(r.embed["color"], 0x5865f2);
        assert!(r.embed["description"]
            .as_str()
            .unwrap()
            .contains("requested your review"));
        assert_eq!(r.embed["url"], "https://gh.test/rc/1");
        assert!(r.embed["title"]
            .as_str()
            .unwrap()
            .starts_with("acme/widgets#12:"));
    }

    #[test]
    fn includes_excerpt_field_when_present() {
        let r = render(&event(EventKind::Mentioned));
        let fields = r.embed["fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f["name"] == "Comment"));
    }

    #[test]
    fn possessive_reflects_authorship() {
        // You only review it → the author's name, never "your PR".
        let d = render(&event(EventKind::Merged)).embed["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(d.contains("merged octo's PR"), "got {d}");
        assert!(!d.contains("your PR"));
        // You authored it → "your PR".
        let mut mine = event(EventKind::Merged);
        mine.viewer.is_author = true;
        let d2 = render(&mine).embed["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(d2.contains("merged your PR"), "got {d2}");
        // The author acted on their own PR → "their own PR", no repeated name.
        let mut own = event(EventKind::Merged);
        own.actor = Actor::new("octo"); // same as the PR author
        let d3 = render(&own).embed["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(d3.contains("merged their own PR"), "got {d3}");
    }

    #[test]
    fn digest_lists_each_event() {
        let events = [event(EventKind::Merged), event(EventKind::Mentioned)];
        let r = render_digest(&events);
        assert_eq!(r.content, "navi digest: 2 updates");
        let desc = r.embed["description"].as_str().unwrap();
        assert!(desc.contains("merged octo's PR"));
        assert!(desc.contains("mentioned you"));
    }

    #[test]
    fn merge_queue_names_the_author_not_their_own_pr() {
        // No actor precedes the phrase here, so "their own PR" would read oddly even
        // when the author enqueued their own PR — name them instead.
        let mut own = event(EventKind::EnteredMergeQueue);
        own.actor = Actor::new("octo"); // author enqueued their own PR
        let d = render(&own).embed["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(d.contains("octo's PR entered the merge queue"), "got {d}");
        assert!(!d.contains("their own PR"), "got {d}");
        // Your own PR still reads "your PR".
        let mut mine = event(EventKind::EnteredMergeQueue);
        mine.viewer.is_author = true;
        let d2 = render(&mine).embed["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(d2.contains("your PR entered the merge queue"), "got {d2}");
    }
}
