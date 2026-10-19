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
- **One TV, one remote** (revised after first deploy, optimizing for simplicity and reliability).
  One tuner, so at most one stream; everyone signed in watches it. One tab holds the remote; others
  watch with the lock banner. "Viewers at once" is gone.
- **Priority.** At most one account (any account, including the admin) is priority. Whenever one of
  its tabs is watching it holds the remote — simply opening the app takes it, with **no retune**
  (the channel only changes when the priority user changes it). The previous holder keeps watching
  as a follower. Jump-to keywords only tune when nothing is playing.
- **Remote hand-off:** when the holder leaves or presses ✕, priority first, then longest-waiting.
- **No external-access restriction.** All accounts may sign in from anywhere.

## Tuner-friendly
- Taking the remote never retunes. A channel change is exactly one restart: the old ffmpeg (and its
  tvheadend subscription) is killed and awaited before the new one starts.
- Existing debounce (0.7 s), single auto-retry, and the 20 s idle grace still apply.

## Also
- `tvh-proxy reset-admin` (run in the container) sets a new random admin password and prints it — the
  way back in if the admin password is lost, without wiping other accounts.
- Deleting an account or resetting its password signs that account out everywhere immediately.
