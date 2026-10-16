# GitHub setup

tern reads your organization's pull requests, reviews, comments and commits
as a **GitHub App**. An App isn't tied to anyone's personal account, can be
limited to read-only access, and the org chooses which repositories it sees.
tern never writes anything to GitHub.

You can create a new App for tern (section 1), or reuse the one dispatchd
already has (section 1b). Either way you need to be an **owner** of the
organization.

## 1. Create the GitHub App

1. Go to your organization's **Settings → Developer settings → GitHub Apps**
   and click **New GitHub App**.
2. Fill in:
   - **GitHub App name**: anything unique on GitHub, e.g. `tern-<your-org>`.
   - **Homepage URL**: required by the form; any URL works, e.g.
     `https://github.com/oxGrad/tern`.
   - **Webhook**: **uncheck Active**. tern polls GitHub every 15 minutes and
     runs on a laptop with no public address.
3. Under **Permissions**, set exactly these and leave everything else at
   **No access**:

   | Section | Permission | Access | What tern reads with it |
   | --- | --- | --- | --- |
   | Repository | Pull requests | Read-only | PRs, reviews, PR commits, inline review comments |
   | Repository | Contents | Read-only | Commits pushed straight to the default branch |
   | Repository | Issues | Read-only | Conversation comments on PRs (GitHub lists this endpoint under both Issues and Pull requests) |
   | Repository | Metadata | Read-only | The repositories the App can see (selected automatically) |
   | Organization | Members | Read-only | Who is on the team in `config.toml` |

4. **Where can this GitHub App be installed?**: **Only on this account**.
5. Click **Create GitHub App**.
6. On the settings page that opens, note the **App ID** (a number near the
   top). That's `app_id` below.
7. Scroll to **Private keys** and click **Generate a private key**. Your
   browser downloads a `.pem` file. **This is the secret**: anyone holding it
   can read whatever the App can. Don't commit it, paste it into chat, or put
   it in `config.toml`.

Then go to step 2.

## 1b. Or reuse dispatchd's App

dispatchd's App only has **Issues**, **Pull requests** and **Metadata**. To
share it with tern:

1. Open the App's settings (org **Settings → Developer settings → GitHub
   Apps → Edit**), go to **Permissions & events**, and add:
   - **Repository → Contents**: Read-only
   - **Organization → Members**: Read-only

   Save. GitHub asks the org to approve the new permissions: open org
   **Settings → GitHub Apps → Configure** for the App and click **Review
   request → Accept new permissions**. Until you do, tern fails with HTTP
   403 on the team and commit endpoints.
2. Under **Private keys**, click **Generate a private key** to get a key just
   for tern. An App can have several keys, so tern's laptop and dispatchd's Pi
   each keep their own, and you can revoke one without touching the other.
   Don't copy dispatchd's encrypted credential off the Pi.

Both tools then share one installation, so they also share its repository
selection (step 2) and its rate limit. tern's steady-state sync costs a few
dozen requests per run, far below the installation's limit.

## 2. Install the App on your organization

Skip this if you reused dispatchd's App; it's already installed.

1. On the App's settings page, open **Install App** in the sidebar and click
   **Install** next to your organization.
2. Choose which repositories the App can read: **All repositories**, or
   **Only select repositories** and pick them.

The installation decides what tern *can* read. To narrow it further without
touching GitHub, list repos in `config.toml` (`repos = ["api", "web"]`).
Archived repos are always skipped. You can change the installation's
selection any time from the org's **Settings → GitHub Apps → Configure**.

## 3. Store the private key

Move the `.pem` somewhere outside any git repository, make it readable only
by you, and point tern at it:

```sh
mv ~/Downloads/tern-your-org.2026-10-16.private-key.pem ~/.config/tern/app.pem
chmod 600 ~/.config/tern/app.pem
export TERN_GITHUB_KEY_PATH=~/.config/tern/app.pem   # add to your shell profile
```

Only the sync worker reads the key; it never reaches the browser.

## 4. Configure tern

Run `tern init` if you haven't yet, then set the `[github]` section in
`~/.config/tern/config.toml` (all three keys are needed together):

```toml
[github]
app_id = 123456
org = "your-org"
team = "engineering"           # the team's slug, as in github.com/orgs/your-org/teams/<slug>
# repos = []                   # empty = every non-archived repo the App can see
# sync_interval_minutes = 15
# api_url = "https://api.github.com"   # GitHub Enterprise: https://<host>/api/v3
```

`team` is the **slug** from the team's URL, not its display name: a team
called "Platform Engineering" usually has the slug `platform-engineering`.

In `~/.config/tern/members.toml`, list the people on the dashboard by GitHub
username:

```toml
[[members]]
github_user = "budi-dev"
name = "Budi"

[[members]]
github_user = "ayu"
name = "Ayu"
in_score = false               # works mostly outside GitHub (e.g. the designer)
emails = ["ayu@personal.dev"]  # commit emails GitHub can't link to the account
```

Anyone on the team but missing from `members.toml` still shows up, under their
login, after the first sync.

## 5. Check it

```sh
tern validate
```

checks both files and the key without calling GitHub:

```text
config.toml: ok (/home/you/.config/tern/config.toml)
github: configured (app 123456 on your-org, team engineering, sync every 15 min)
key: ok (/home/you/.config/tern/app.pem)
members.toml: ok (5 members, 4 in score - /home/you/.config/tern/members.toml)
```

Then

```sh
tern status
```

signs in as the App, finds its installation, gets a token and counts the
repositories it can see:

```text
github:
  app:                  123456 on your-org
  key:                  ok (/home/you/.config/tern/app.pem)
  api:                  ok - "tern-your-org" installed (selected repositories), 14 repos visible, 182ms
sync:
  last run:             never - run: tern sync --once
```

Pull the history, then start the daemon:

```sh
tern backfill --since 2026-01-01
tern
```

## Rotating or revoking the key

Generate a new key on the App's settings page, replace the file at
`TERN_GITHUB_KEY_PATH`, run `tern status` to confirm it works, then delete the
old key on GitHub. To stop syncing, remove the `[github]` section; the
dashboard keeps serving what's already in `tern.db`.

## Troubleshooting

| Message | Fix |
| --- | --- |
| `key: not set - run: export TERN_GITHUB_KEY_PATH=...` | Step 3. The variable must be set in the shell that runs `tern`. |
| `key: failed - ... is missing or empty` | `TERN_GITHUB_KEY_PATH` points at the wrong file. |
| `key: failed - not a PEM file` | That file isn't the App's private key. Download a new one (step 1.7). |
| `GitHub rejected the App JWT (401)` | `app_id` doesn't match the App the key belongs to, or the laptop's clock is off. |
| `the App is not installed on the "x" org (404)` | Install it (step 2), or fix `org`. |
| `GitHub refused to issue an installation token` | The installation is suspended. Unsuspend it in the org's **Settings → GitHub Apps**. |
| `the installation token can't list repositories (403)` | The App is missing **Metadata: Read-only**. |
| `GET .../teams/<team>/members failed with HTTP 404` or `403` | `team` isn't the team's slug (copy it from the team's URL), or the App lacks **Organization → Members: Read-only**, or the org hasn't accepted the new permissions yet (step 1b). GitHub often answers 404 rather than 403 for a missing permission. |
| `failed to sync <repo>: GET .../commits failed with HTTP 403` | The App lacks **Contents: Read-only**, or the new permissions aren't accepted yet. |
| `14 repos visible` but nothing syncs | `repos` in `config.toml` doesn't match: use `name` or `owner/name`. |
| `0 repos visible` | The installation has no repositories selected (step 2). |
| `rate limited: ...` (run marked `partial`) | Normal during a big backfill. Repos that finished keep their progress; the one that hit the limit starts over on the next run (every 15 min). |
