//! Render a navi Event into a Discord message (content + one embed).

use navi_notifier_core::model::{Event, EventKind, ReviewState};
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

/// One-line headline plus an embed color for the event kind.
fn headline(event: &Event, actor: &str) -> (String, u32) {
    match &event.kind {
        EventKind::ReviewRequested => (format!("👀 **{actor}** requested your review"), 0x5865f2),
        EventKind::ReReviewRequested => (format!("🔁 **{actor}** requested a re-review"), 0x5865f2),
        EventKind::ReviewSubmitted { state } => match state {
            ReviewState::Approved => (format!("✅ **{actor}** approved your PR"), 0x2ecc71),
            ReviewState::ChangesRequested => {
                (format!("⚠️ **{actor}** requested changes"), 0xe67e22)
            }
            ReviewState::Commented => (format!("💬 **{actor}** left a review comment"), 0x3498db),
        },
        EventKind::ReviewDismissed => (format!("♻️ **{actor}** dismissed your review"), 0x95a5a6),
        EventKind::CommentReply { on_your_comment } => {
            let text = if *on_your_comment {
                format!("💬 **{actor}** replied to your comment")
            } else {
                format!("💬 **{actor}** replied in a thread you're in")
            };
            (text, 0x3498db)
        }
        EventKind::Mentioned => (format!("👋 **{actor}** mentioned you"), 0xf1c40f),
        EventKind::Merged => (format!("🟣 **{actor}** merged your PR"), 0x9b59b6),
        EventKind::Closed => ("🚫 Your PR was closed".to_string(), 0xe74c3c),
        EventKind::ReadyForReview => (
            format!("🚀 **{actor}** marked a PR ready for review"),
            0x2ecc71,
        ),
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
}
