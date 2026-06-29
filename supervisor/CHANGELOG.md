# Changelog

All notable changes to `embassy-supervisor` are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0]

Initial release.

- Dependency-ordered task bring-up and reverse-ordered teardown over a `task_graph!` of `TaskNode`s
  (topological sort, no allocation).
- Lifecycle modes: `Terminate`, `Pause`, `OnDemand`.
- Elastic worker pools (`ElasticPool` with a swappable `ScalingPolicy`, e.g. `DeferredShrink`)
  behind the `pool` feature.
- Decoupled runtime start/stop/pause/resume control (`request_control` / `apply_control`) behind the
  `control` feature.
- Optional `defmt` logging behind the `defmt` feature (no-op otherwise).

[Unreleased]: https://github.com/cedrivard/embassy-supervisor/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/cedrivard/embassy-supervisor/releases/tag/v0.1.0
