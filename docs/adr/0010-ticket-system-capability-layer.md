# ADR 0010 · Ticket systems as a normalized work-item capability layer

- **Status:** accepted
- **Date:** 2026-06-14

## Context

Caliban and the sibling repos increasingly reason over tracked work — issues,
Kanban cards, ADR-linked tasks — that today lives entirely outside Gonzalo. We
want that work to be a first-class `Record`: versioned, synced, conflict-merged,
and composable with the vector and graph layers the same way memory and sessions
are. Gonzalo has no such abstraction today (no `Ticket` kind, no source trait).

The hazard is the one ADR 0004 flags for `Store`: a lowest-common-denominator
trait that *flattens* real differences, or one that accretes provider-specific
escape hatches until it is not an abstraction. Ticket platforms diverge more than
storage backends do, so this risk is acute. We surveyed nine platforms chosen to
cover distinct data-model archetypes, not for breadth's sake:

- **GitHub** — open/closed + reason; Projects v2 status is a per-project field.
- **Jira** — custom workflow → `statusCategory`; **transition-gated** writes; ADF body.
- **Linear** — typed states (backlog/started/completed/canceled); direct set.
- **GitLab** — **tier-dependent:** free = open/closed + `workflow::*` scoped
  labels; Premium = native categorized Status; custom fields GA in 18.0.
- **Asana** — **no intrinsic state** (`completed` bool / section / enum field);
  **multi-homed** across projects; single assignee; plaintext/HTML body.
- **Azure DevOps** — work-item **type** (process template) drives fields *and*
  states; states grouped into categories; any-to-any transitions by default.
- **Bugzilla** — **two-dimensional** state: `status` **×** `resolution`
  (FIXED / WONTFIX / DUPLICATE / INVALID …).
- **Monday / Airtable** — **fully schemaless**: title, status, assignee are all
  user-named columns/fields.
- **Zendesk / ServiceNow** — support/ITSM: distinct requester/assignee/submitter,
  a `pending` state, cross-type links (incident→problem→change), SLA events.

Two facts fell out of the survey. First, a **categorized status** model is shared
by six of the nine (Jira, Linear, GitLab-Premium, Azure DevOps, ClickUp,
Shortcut) — so a normalized state *category* is a real spine, not a forced fit.
Second, the *signal* that carries status is configured **per connection**, not
fixed per provider (GitLab free vs Premium; Asana completed vs section vs field).

## Decision

Model tickets as a **capability layer over core** (ADR 0008), not a new core
*concept* (ADR 0002). The only core change is registering two new `RecordKind`
variants (`Ticket`, `TicketEvent`) and their merge classes — the same minimal
touch every domain kind already requires; no new core traits or types enter.

- **Record shape.** New `RecordKind`s — a ticket and an append-only
  ticket-event/comment stream — with a typed `Ticket` view in `gonzalo-domain`,
  mapped to/from `Record` via serde, exactly as `MemoryTier`/`Session` are.
- **Normalized canonical model + lossless raw.** `Ticket` carries an `item_type`;
  a `State { category, resolution, raw_name, raw_id }` (category is the
  cross-platform spine, `resolution` the second axis Bugzilla/Jira need, raw round-
  trips); actor *roles* (`Requester | Assignee | Submitter | Follower`);
  normalized common fields (title, body, priority, labels); many-to-many
  `containers` (Asana multi-home, multi-board); typed `links` (blocks/relates/
  parent/duplicate → `RecordKey` or external ref); a `Body { markdown, raw,
  format: Markdown|Adf|Html|PlainText }`; and a bounded `fields` map for everything
  else. `StateCategory` includes a `Pending`/blocked member.
- **`TicketSource` trait** is the provider boundary — the ticket analogue of
  `Embedder`, keeping Gonzalo provider-agnostic about *where* tickets come from.
- **Per-connection mapping policy.** Because state and fields are instance-
  configured, each connection carries a `FieldMapping`/`StateMapping` (the
  generalization of a fixed per-provider schema) that resolves canonical fields
  and the normalized state category from the configured signal — intrinsic state,
  scoped label, native status, section, or custom field.
- **Capability negotiation, not escape hatches.** A `capabilities()` descriptor
  (`push`, `transitions_required`, `custom_fields`, `single_assignee`,
  `hierarchy`, `relations`, `comments`) replaces `if provider == …` branches;
  available writes may be dynamic (auth/workflow-dependent).
- **Reuse concurrency + merge (ADR 0005).** Ticket state is a structured kind →
  field-level 3-way merge; the event/comment stream is append-only → union-merge.
- **Opaque incremental `Cursor`** owned by each source (timestamp / JQL bound /
  GraphQL cursor / event sync token) — *not* Gonzalo's `Revision`.
- **Read-only import first; capability-gated write-back second.** `fetch_changed`
  is uniform across every platform and tier and already delivers the composition
  value; `set_state(category)` (which the source resolves to a Jira transition, a
  GitLab label swap, or an Asana section move) is opt-in phase 2.
- **Conformance keyed on policy variants** (ADR 0006), not just providers:
  "GitLab-free (scoped-label)" and "GitLab-Premium (native status)" are distinct
  fixture sets, run against recorded fixtures like the S3/daemon substrates.
- **Scope boundary.** Schemaless DB tools (Monday/Airtable) are *supportable* via
  `FieldMapping` but are not design drivers; alerting/error-aggregation tools
  (PagerDuty/Sentry) are out of scope — they are not human-authored tickets.
- **Composition.** Because tickets key off `RecordKey`, ticket ⋈ graph (which
  symbols/files a ticket touches) and ticket ⋈ vector (semantic search over
  bodies) fall out for free, returning first-class records (ADR 0008).

## Consequences

- **Positive:** Tracked work becomes a first-class `Record` — versioned, synced,
  conflict-aware — with only the minimal `RecordKind` registration every domain
  kind needs. The normalized model is validated across
  nine platforms and nine archetypes, so adding a tenth (Shortcut, ClickUp,
  Azure DevOps variants) is "implement one trait + declare a mapping policy +
  capabilities + fixtures." Read-only-first confines all per-instance write risk
  to an opt-in phase.
- **Negative:** The canonical model is wide (item_type, two-axis state, actor
  roles, many-to-many containers, raw passthrough) — more surface than a
  GitHub-issue clone. Per-connection `FieldMapping` is configuration users must
  get right, especially for schemaless tools. Two `RecordKind`s per ticket
  (state + events) is more mapping than a single document.
- **Revisit if:** a platform cannot be expressed even with `FieldMapping` +
  `fields` (revisit the canonical shape); two-way sync conflict between Gonzalo's
  merge and a remote's authoritative state proves intractable (reconsider write-
  back); or the layer needs core/substrate support a pure layer cannot provide.
