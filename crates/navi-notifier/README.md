# navi-notifier

A friendly helper to guide you through the day-to-day noise of code review.
Sources: GitHub, GitLab, Gitea/Forgejo. Destinations: Slack, Discord, email.

Installs a binary called `navi`.

```sh
cargo install navi-notifier --locked
navi init
navi run
```

navi sends you a tight, high-signal stream of only the review events that matter (review requests, re-reviews,
replies to your comments, dismissals, mentions, merges, and closes) without the noise of a forge's native
notifications. Every alert kind is toggle-able and filterable by repo.

See the [project README](https://github.com/lararosekelley/navi#readme) for setup, configuration, and
architecture.

## License

[MIT](https://github.com/lararosekelley/navi/blob/main/LICENSE).
