# Host workspaces and logical sessions

Status: implemented after v0.3. Isolated-fixture coverage for every sizing and
ordinary-client edge case remains open.

## Decision

Starcom should present a **session** as its own named, persistent workspace while
implementing that session as one tmux window. All Starcom sessions on one server
belong to one managed tmux session, named `starcom` by default, and use one SSH
connection and one tmux control client while any of them are open locally.

```text
Starcom server workspace
  one resolved SSH route and authenticated transport
    one `tmux -N -C attach-session -t =starcom` control client
      tmux window "api"       -> Starcom tab "api"
      tmux window "frontend"  -> Starcom tab "frontend"
      tmux window "release"   -> Starcom tab "release"
```

The managed tmux session is an implementation detail, but not a secret. Errors,
advanced configuration, and fallback instructions may name it. A user must still
be able to run `tmux attach-session -t =starcom` from an ordinary terminal.

The default belongs on the user's normal tmux server rather than a dedicated
socket. A dedicated socket would isolate Starcom but would also start another
tmux server process and make fallback/discovery less obvious. The advanced socket
setting remains supported. A configurable managed-session name can follow when a
real collision or multi-workspace use case requires it; changing it is not needed
to establish the ownership model.

## Why this is preferable

The v0.3 model owns one SSH connection and one remote tmux **client** process per
Starcom tab. Those clients normally attach to different sessions on the same
existing tmux server; Starcom does not normally create one tmux server process per
tab. Ten tabs can therefore mean ten SSH connections, ten sshd children, and ten
tmux control clients even though they are one conceptual server workspace.

A single control client already receives pane output for every window in its
attached session. The target model removes the redundant persistent transports
and remote clients, shares one coherent reconnect epoch per server, and matches
the UI: the user manages named workspaces, not SSH or tmux attachment objects.

Pooling several independent control channels onto one Sunset connection is not
the primary solution. It would remove TCP connections but retain a remote tmux
client per tab, duplicate snapshots, and give one transport a host-wide failure
blast radius without giving Starcom a host-wide owner. Sunset currently consumes
a connection into one exec/subsystem channel. It need not change for the first
host-workspace implementation.

## Ownership and identity

A server workspace is keyed by the fully resolved route, not only by the label in
the tab or by a hostname. The key includes destination host, user, port, jump
route, host-key policy, authentication policy, and tmux socket. Connections with
different security or routing semantics must never be merged merely because they
end at the same address.

The server workspace owns:

- the SSH/control worker, cancellation wake, reconnect schedule, and connection
  epoch;
- the tmux server/session identity and the single reconstructed view containing
  all managed windows;
- the set of logical tabs currently open in Starcom;
- shared client geometry and remote-resize consent.

A logical tab owns:

- a unique tmux window name plus the last observed window ID;
- selected pane, local history position, selection, and transient UI state;
- its activity/quiet marker and ordering in the local tab strip.

Window IDs are authoritative within a live tmux server. The saved name is the
resume target. On reconnect, Starcom validates the saved ID and name against a
fresh snapshot. A different server/session identity is a replacement; the same
name with a new window ID on the same server means that logical session was
replaced. Tmux permits duplicate window names, so Starcom must enforce unique
names inside its managed session rather than accepting an ambiguous target.

## Lifecycle

Selecting a server follows one of three paths:

1. If its server workspace is already live, list windows from its current view;
   no SSH operation is needed.
2. If the managed tmux session exists, attach one control client and list its
   windows through that client.
3. If it does not exist, show an empty list. Only an explicit **Create** action may
   create the managed tmux session and its first window, after which Starcom
   attaches normally.

Further **Create** actions issue `new-window` through the existing control client.
Rename issues `rename-window`. Closing a Starcom tab removes only that local view;
it does not kill the tmux window or its jobs. Closing the last local tab detaches
the host control client. A separate, confirmed action may eventually delete a
remote session/window; **Exit** must not acquire that meaning.

The composer lists all managed windows and marks names already open in this local
workspace red and unavailable, preventing duplicate logical tabs. An ordinary
tmux client may still attach to the managed session as a fallback; its presence
does not make every window unavailable.

Existing arbitrary tmux sessions do not silently disappear during migration.
Workspace format v3 marks new tabs as managed windows; v1/v2 entries are loaded
as explicit legacy-session attachments to their original tmux session. They keep
the old one-attachment behavior until the user closes them. A future import/link
operation may move them into the managed session, but it needs fixture coverage
for naming, last-link deletion, and shared-layout effects first.

## Correctness implications

- A transport loss freezes and reconnects every tab on that server together.
  The UI should report one server-level failure rather than N independent retry
  countdowns.
- Every input action remains bound to the host epoch, reconstructed generation,
  window, and pane. No action captured in one logical tab may be redirected to
  another window after a rename, move, reconnect, or replacement.
- One control client size applies to the managed workspace. This matches one
  visible application viewport, but switching tabs and remote-resize consent
  need explicit tests with ordinary tmux clients attached.
- History depth becomes a server-workspace policy. The first implementation may
  use the maximum requested depth across its tabs, within the existing bounds.
- Snapshot and pane budgets currently apply per attached tmux session. They will
  become per server workspace and must be measured against 5–10 windows before
  defaults are changed.
- SFTP uploads may continue to use short-lived SSH connections. They are bounded
  and transient; sharing them is a later Sunset channel-multiplexing improvement,
  not a prerequisite for eliminating persistent per-tab connections.

## Implementation order

1. Add window names and stable window identity to snapshots and reconnect tests.
2. Introduce a server-workspace owner and make multiple logical tabs share one
   in-memory view without sharing mutable per-tab selection state.
3. Change discovery/create/rename from tmux sessions to windows under the managed
   session, keeping creation explicit and exact-targeted.
4. Group saved tabs by resolved server key during restore and reconnect once per
   group. Migrate or explicitly reject ambiguous v0.3 entries.
5. Add isolated SSH/tmux fixtures for duplicate names, window replacement,
   last-tab detach, multi-window output, transport loss, sizing, and fallback.
6. Only then consider reusable Sunset connections for transient SFTP or other
   independent channels.
