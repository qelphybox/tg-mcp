# Writing a scenario

A scenario is a plain async function in `acceptance/src/scenarios/<id>.rs`, registered in `scenarios::ALL`. Its id matches the `docs/acceptance.md` section it replaces (`s53`), or names what it proves (`login`, `smoke`). `scenarios/s53.rs` is the worked example.

## What it uses

- `run.login(&["draft"])`: the Login flow with those Scopes ticked (`read` always), the Driver accepting the QR, the Cloud password typed, and the interlock on `whoami`. `run.login_as(Who::Peer, …)` for the Peer.
- `session.call(tool, args)` for a result; `session.try_call` when the scenario expects the tool's error (it comes back as `Err(message)`).
- `Fixtures::resolve(&session, run.world.peer_account)`: the test chats by role (A, B, the forum and its Releases Topic, G, the folder Work, Saved Messages).
- `run.world.driver`: what Telegram really holds on the Test account (Drafts, with entities); `run.world.peer`: the other side.
- `run.mark(text)` on every text the scenario writes: the run id is how assertions find their own texts and how the logs and database check finds leaks.
- `run.passed(step)` after each proven step; `run.human(question, expected)` for what only a person can judge, with the state left in place for them.

## Rules

- Assert on what Telegram holds through the Driver, not only on what the server answers.
- Leave Saved Messages alone: it holds the run's lease.
- Clean up what the run wrote, except what a Human check still needs.
- A scenario runs on both worlds. When the fake Telegram can't show something (it takes no incoming messages at run time), the fake side says so in a comment and the scenario still passes on it.
- Once the scenario passes, shrink its `docs/acceptance.md` section to: how to run it, what it checks, its Human checks, and what stays by hand.
