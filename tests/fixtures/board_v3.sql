-- A board exactly as 0.2.x and 0.3.0 left it (schema version 3): task ids are a
-- plain INTEGER PRIMARY KEY, tasks 4 and 6 were removed, and every table that
-- points at `tasks` has rows. tests/upgrade.rs upgrades it and checks that
-- nothing is lost.
CREATE TABLE agents (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  role TEXT NOT NULL DEFAULT 'developer' CHECK (role IN ('developer','reviewer')),
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  priority TEXT NOT NULL CHECK (priority IN ('low','medium','high','urgent')),
  status TEXT NOT NULL DEFAULT 'todo' CHECK (status IN ('backlog','todo','in_progress','review','done')),
  executor INTEGER REFERENCES agents(id),
  tags TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(tags)),
  tests TEXT NOT NULL CHECK (json_valid(tests) AND json_array_length(tests) > 0),
  revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
  claimed_at TEXT,
  lease_expires_at TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  CHECK (
    (executor IS NULL AND claimed_at IS NULL AND lease_expires_at IS NULL)
    OR
    (executor IS NOT NULL AND claimed_at IS NOT NULL AND lease_expires_at IS NOT NULL)
  ),
  CHECK (status != 'done' OR executor IS NULL)
);

CREATE TABLE review_history (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  revision INTEGER NOT NULL CHECK (revision > 0),
  decision TEXT NOT NULL CHECK (decision IN ('approved','changes_requested')),
  notes TEXT NOT NULL DEFAULT '',
  executor INTEGER REFERENCES agents(id) ON DELETE SET NULL,
  executor_name TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (task_id, revision)
);

CREATE TABLE acceptance_results (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  revision INTEGER NOT NULL CHECK (revision > 0),
  criterion_index INTEGER NOT NULL CHECK (criterion_index >= 0),
  criterion TEXT NOT NULL CHECK (json_valid(criterion)),
  result TEXT NOT NULL CHECK (result IN ('passed','failed')),
  evidence TEXT NOT NULL CHECK (length(trim(evidence)) > 0),
  executor INTEGER REFERENCES agents(id) ON DELETE SET NULL,
  executor_name TEXT NOT NULL,
  verified_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (task_id, revision, criterion_index)
);

CREATE TABLE task_deps (
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  depends_on INTEGER NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
  PRIMARY KEY (task_id, depends_on),
  CHECK (task_id != depends_on)
);

CREATE INDEX review_history_task_revision ON review_history(task_id, revision);
CREATE INDEX acceptance_results_task_revision
  ON acceptance_results(task_id, revision, criterion_index);
CREATE INDEX task_deps_depends_on ON task_deps(depends_on);

INSERT INTO agents (id, name, role) VALUES (1, 'alice', 'developer'), (2, 'rita', 'reviewer');

INSERT INTO tasks (id, title, priority, status, tags, tests, revision) VALUES
  (1, 'base', 'high', 'done', '["core"]',
   '[{"describe":"parses","input":"a","output":"b"}]', 2),
  (2, 'next', 'medium', 'todo', '[]',
   '[{"describe":"d","input":"i","output":"o"}]', 0),
  (3, 'in review', 'low', 'review', '[]',
   '[{"describe":"d","input":"i","output":"o"}]', 1),
  (7, 'newest', 'low', 'backlog', '[]',
   '[{"describe":"d","input":"i","output":"o"}]', 0);
INSERT INTO tasks (id, title, priority, status, executor, claimed_at, lease_expires_at, tags, tests)
  VALUES (5, 'held', 'urgent', 'in_progress', 1, datetime('now'), datetime('now', '+1 hour'), '[]',
          '[{"describe":"d","input":"i","output":"o"}]');

INSERT INTO review_history (task_id, revision, decision, notes, executor, executor_name) VALUES
  (1, 1, 'changes_requested', 'fix it', 2, 'rita'),
  (1, 2, 'approved', 'good', 2, 'rita');

INSERT INTO acceptance_results
  (task_id, revision, criterion_index, criterion, result, evidence, executor, executor_name) VALUES
  (1, 1, 0, '{"describe":"parses","input":"a","output":"b"}', 'failed', 'red', 1, 'alice'),
  (1, 2, 0, '{"describe":"parses","input":"a","output":"b"}', 'passed', 'green', 1, 'alice'),
  (3, 1, 0, '{"describe":"d","input":"i","output":"o"}', 'passed', 'ok', 1, 'alice');

INSERT INTO task_deps (task_id, depends_on) VALUES (2, 1), (7, 5);

PRAGMA user_version = 3;
