-- src/db/migrations/0001_initial.sql (foreign_keys and WAL are set per connection in db::open)

-- Team roster, seeded from members.toml on every start
CREATE TABLE members (
    github_user TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    in_score    BOOLEAN NOT NULL DEFAULT TRUE,   -- FALSE for people whose work is mostly outside GitHub
    avatar_url  TEXT,
    is_bot      BOOLEAN NOT NULL DEFAULT FALSE
);

-- Unlinked commit emails, seeded from members.toml (emails = [...])
CREATE TABLE member_emails (
    email       TEXT PRIMARY KEY,
    github_user TEXT NOT NULL,
    FOREIGN KEY (github_user) REFERENCES members(github_user)
);

CREATE TABLE repos (
    id             INTEGER PRIMARY KEY,          -- GitHub repo id
    full_name      TEXT NOT NULL UNIQUE,
    default_branch TEXT NOT NULL,
    archived       BOOLEAN NOT NULL DEFAULT FALSE,
    pushed_at      TEXT
);

CREATE TABLE away_days (
    github_user TEXT NOT NULL,
    date        TEXT NOT NULL,                   -- 'YYYY-MM-DD', local date
    PRIMARY KEY (github_user, date)
);

-- Raw activity, keyed by GitHub ids, written by upsert
CREATE TABLE pull_requests (
    id              INTEGER PRIMARY KEY,         -- GitHub PR id
    repo_id         INTEGER NOT NULL,
    number          INTEGER NOT NULL,
    author          TEXT NOT NULL,
    title           TEXT,
    state           TEXT NOT NULL,               -- 'open' | 'closed'
    draft           BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TEXT NOT NULL,
    first_review_at TEXT,                        -- drives time to first review
    merged_at       TEXT,
    closed_at       TEXT,
    updated_at      TEXT NOT NULL,
    additions       INTEGER,
    deletions       INTEGER,
    changed_files   INTEGER,
    UNIQUE (repo_id, number),
    FOREIGN KEY (repo_id) REFERENCES repos(id)
);

CREATE TABLE commits (
    sha          TEXT PRIMARY KEY,
    repo_id      INTEGER NOT NULL,
    author_login TEXT,                           -- NULL when unlinked; resolved via member_emails
    author_email TEXT,
    authored_at  TEXT NOT NULL,                  -- set by the author's machine
    committed_at TEXT NOT NULL,
    pr_number    INTEGER,                        -- NULL for direct pushes
    additions    INTEGER,
    deletions    INTEGER,
    FOREIGN KEY (repo_id) REFERENCES repos(id)
);

CREATE TABLE reviews (
    id           INTEGER PRIMARY KEY,            -- GitHub review id
    repo_id      INTEGER NOT NULL,
    pr_number    INTEGER NOT NULL,
    reviewer     TEXT NOT NULL,
    state        TEXT NOT NULL,                  -- 'APPROVED' | 'CHANGES_REQUESTED' | 'COMMENTED'
    submitted_at TEXT NOT NULL
);

CREATE TABLE comments (
    id         INTEGER PRIMARY KEY,              -- GitHub comment id
    repo_id    INTEGER NOT NULL,
    pr_number  INTEGER NOT NULL,
    author     TEXT NOT NULL,
    kind       TEXT NOT NULL,                    -- 'review' (inline on code) | 'conversation'
    body_len   INTEGER,
    created_at TEXT NOT NULL
);

-- Rollup the charts read, rebuilt for touched days after each sync. Never pruned.
CREATE TABLE daily_member_stats (
    github_user     TEXT NOT NULL,
    date            TEXT NOT NULL,               -- 'YYYY-MM-DD', Asia/Jakarta
    commits         INTEGER NOT NULL DEFAULT 0,
    prs_opened      INTEGER NOT NULL DEFAULT 0,
    prs_merged      INTEGER NOT NULL DEFAULT 0,
    reviews_given   INTEGER NOT NULL DEFAULT 0,  -- on other people's PRs only
    review_comments INTEGER NOT NULL DEFAULT 0,
    conv_comments   INTEGER NOT NULL DEFAULT 0,
    lines_changed   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (github_user, date)
);

-- Sync bookkeeping: lets any restart resume from the stored cursors
CREATE TABLE sync_state (
    repo_id   INTEGER NOT NULL,
    entity    TEXT NOT NULL,                     -- 'pulls' | 'review_comments' | 'issue_comments' | 'commits'
    cursor_at TEXT,                              -- newest updated_at seen
    etag      TEXT,
    PRIMARY KEY (repo_id, entity)
);

-- Pruned weekly by `tern maintenance run` (90 days)
CREATE TABLE sync_runs (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at     TEXT NOT NULL,
    finished_at    TEXT,
    requests_used  INTEGER,
    rate_remaining INTEGER,
    status         TEXT,                         -- 'ok' | 'partial' | 'failed'
    error          TEXT
);

-- One row per event, for the heatmap and the activity charts
CREATE VIEW activity AS
    SELECT author_login AS github_user, 'commit' AS kind, repo_id, authored_at AS ts FROM commits
    UNION ALL SELECT author, 'pr_opened', repo_id, created_at FROM pull_requests
    UNION ALL SELECT author, 'pr_merged', repo_id, merged_at FROM pull_requests WHERE merged_at IS NOT NULL
    UNION ALL SELECT reviewer, 'review', repo_id, submitted_at FROM reviews
    UNION ALL SELECT author, 'comment_' || kind, repo_id, created_at FROM comments;

CREATE INDEX idx_pr_author   ON pull_requests (author, created_at);
CREATE INDEX idx_commit_auth ON commits (author_login, authored_at);
CREATE INDEX idx_review_rev  ON reviews (reviewer, submitted_at);
CREATE INDEX idx_comment_au  ON comments (author, created_at);
