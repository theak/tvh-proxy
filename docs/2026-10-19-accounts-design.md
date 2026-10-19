# Accounts, priority viewer, full-screen settings

## Intent
Several people use one fragile USB tuner. One admin manages the server; other accounts just watch. One
account can be the *priority* viewer who can take the tuner when they want it. Keeping the tuner
healthy (as few retunes as possible) beats everything else.

## Decisions (from Akshay)
- **Accounts.** The account created during first-run setup is the admin. The admin can add/delete
  other accounts and reset their passwords. Existing installs: the current login becomes the admin;
  sessions carry over.
- **Settings are global and admin-only.** Non-admins see no settings (no gear); they only get Sign out.
- **Settings is a full-screen page**, not a modal. Opening it stops playback (same as ✕).
- **Priority.** At most one account (any account, including the admin) is priority.
  - It takes over **only when the priority user picks a channel** — never just by opening the app;
    jump-to keywords don't pre-empt.
  - The bumped viewer keeps watching as a **follower** of the priority user's channel, with the existing
    lock banner. No new UI.
  - When control is handed on (controller leaves), a waiting priority viewer is promoted first.
- **No external-access restriction.** All accounts may sign in from anywhere.

## Tuner-friendly take-over
- Taking over a stream that's already on the requested channel: change of control only, **no retune**.
- Otherwise exactly one retune of the existing stream: the old ffmpeg (and its tvheadend subscription)
  is killed and awaited *before* the new one starts, so the tuner is never asked for two muxes at once.
- With several slots busy, the priority viewer takes the oldest non-priority stream; a free slot is used
  first if there is one.
- Existing debounce (0.7 s) and one auto-retry still apply.

## Also
- `tvh-proxy reset-admin` (run in the container) sets a new random admin password and prints it — the
  way back in if the admin password is lost, without wiping other accounts.
- Deleting an account or resetting its password signs that account out everywhere immediately.
