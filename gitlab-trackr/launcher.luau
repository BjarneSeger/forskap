-- Noctalia launcher provider for gitlab-trackrd: `/gl <query>`.
--
-- Thin by design, like `tt` itself: every query is one `tt search --output
-- json` run, every activation one `tt open` (issues/MRs — which also records
-- the open so the daemon ranks it higher next time) or `xdg-open`
-- (projects/groups). The daemon owns ranking; results are emitted in its
-- order with `score = open_count`, so frequently opened issues and MRs
-- interleave across kinds instead of one kind always sitting on top.

local CATEGORY = {
  issues = "Issues",
  merge_requests = "Merge requests",
  projects = "Projects",
  groups = "Groups",
}

local GLYPH = {
  issues = "circle-dot",
  merge_requests = "git-pull-request",
  projects = "folder",
  groups = "users",
}

-- Leading word → `tt search --kind` value.
local KIND_WORDS = {
  i = "issues", issue = "issues", issues = "issues",
  mr = "merge_requests", mrs = "merge_requests",
  p = "projects", project = "projects", projects = "projects",
  g = "groups", group = "groups", groups = "groups",
}

local function trim(s)
  return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end

-- Split the text behind the prefix into an optional kind filter and the
-- query handed to `tt search`. Pure, so the shapes are easy to eyeball:
--   "oauth"        → nil,              "oauth"
--   "mr oauth"     → "merge_requests", "oauth"
--   "mr"           → "merge_requests", ""        (frequently opened MRs)
--   "!42"          → "merge_requests", "#42"     (`tt` only knows the # form)
--   "#42"          → "issues",         "#42"
local function parseQuery(text)
  local t = trim(text or "")
  local word, rest = t:match("^(%S+)%s*(.*)$")
  if word ~= nil and KIND_WORDS[word:lower()] ~= nil then
    return KIND_WORDS[word:lower()], trim(rest)
  end
  local mrNumber = t:match("^!(%d+)$")
  if mrNumber ~= nil then
    return "merge_requests", "#" .. mrNumber
  end
  if t:match("^#%d+$") then
    return "issues", t
  end
  return nil, t
end

-- The configured path wins; otherwise `tt` from noctalia's PATH, falling back
-- to the usual cargo/user bin dirs a login shell has but the compositor
-- session often lacks.
local function ttPath()
  local configured = noctalia.getConfig("tt_path")
  if configured ~= nil and configured ~= "" and configured ~= "tt" then
    return noctalia.expandPath(configured)
  end
  if noctalia.commandExists("tt") then
    return "tt"
  end
  for _, dir in ipairs({ "~/.cargo/bin/", "~/.local/bin/" }) do
    local candidate = noctalia.expandPath(dir .. "tt")
    if noctalia.fileExists(candidate) then
      return candidate
    end
  end
  return "tt"
end

-- "https://gl/team/api/-/issues/42" → "team/api"
local function projectPath(webUrl)
  local path = (webUrl or ""):match("^https?://[^/]+/(.-)/%-/")
  return path or ""
end

local function joinNonEmpty(parts)
  local kept = {}
  for _, p in ipairs(parts) do
    if p ~= nil and p ~= "" then
      table.insert(kept, p)
    end
  end
  return table.concat(kept, " · ")
end

local function issuableRow(kind, sigil, item)
  return {
    id = string.format("%s:%d:%d", kind, item.project_id, item.iid),
    title = string.format("%s%d %s", sigil, item.iid, item.title),
    subtitle = joinNonEmpty({ projectPath(item.web_url), item.state, item.total_time }),
    glyph = GLYPH[kind],
    category = CATEGORY[kind],
    score = item.open_count or 0,
  }
end

local function urlRow(kind, item)
  return {
    id = "url:" .. (item.web_url or ""),
    title = item.path,
    subtitle = item.name,
    glyph = GLYPH[kind],
    category = CATEGORY[kind],
  }
end

local function toResults(reply)
  local results = {}
  for _, i in ipairs(reply.issues or {}) do
    table.insert(results, issuableRow("issues", "#", i))
  end
  for _, m in ipairs(reply.merge_requests or {}) do
    table.insert(results, issuableRow("merge_requests", "!", m))
  end
  for _, p in ipairs(reply.projects or {}) do
    table.insert(results, urlRow("projects", p))
  end
  for _, g in ipairs(reply.groups or {}) do
    table.insert(results, urlRow("groups", g))
  end
  return results
end

local function firstLine(s)
  local line = (s or ""):match("^[^\n]*")
  -- `tt` prefixes anyhow errors with "Error: ".
  return trim((line or ""):gsub("^Error:%s*", ""))
end

local function messageRow(title, glyph)
  return { { id = "message", title = title, glyph = glyph } }
end

function onQuery(text)
  local kind, query = parseQuery(text)
  local argv = { ttPath(), "search", "--output", "json" }
  local limit = noctalia.getConfig("limit")
  if limit ~= nil then
    table.insert(argv, "--limit")
    table.insert(argv, tostring(limit))
  end
  if kind ~= nil then
    table.insert(argv, "--kind")
    table.insert(argv, kind == "merge_requests" and "mrs" or kind)
  end
  if query ~= "" then
    table.insert(argv, query)
  end

  launcher.setResults(text, messageRow("Searching…", "loader"))
  local started = noctalia.runAsync(argv, function(result)
    if result.exitCode ~= 0 then
      local why = firstLine(result.stderr)
      if why == "" then
        why = "tt exited with " .. tostring(result.exitCode)
      end
      launcher.setResults(text, messageRow(why, "alert-circle"))
      return
    end
    local reply, err = noctalia.json.decode(result.stdout)
    if reply == nil then
      launcher.setResults(text, messageRow("Unreadable tt output: " .. tostring(err), "alert-circle"))
      return
    end
    local results = toResults(reply)
    if #results == 0 then
      local hint = query == "" and "Nothing opened yet — search, then open something" or "No matches"
      results = messageRow(hint, "search")
    end
    launcher.setResults(text, results)
  end)
  if not started then
    launcher.setResults(text, messageRow("Could not start tt — check the tt binary setting", "alert-circle"))
  end
end

function onActivate(id)
  if id == "message" then
    return
  end
  local url = id:match("^url:(.+)$")
  if url ~= nil then
    if not noctalia.runAsync({ "xdg-open", url }) then
      noctalia.notifyError("GitLab Trackr", "Could not open " .. url)
    end
    return
  end
  local kind, projectId, iid = id:match("^([%a_]+):(%d+):(%d+)$")
  if kind == nil then
    return
  end
  local argv = { ttPath(), "open", iid, "-p", projectId }
  if kind == "merge_requests" then
    table.insert(argv, "--mr")
  end
  noctalia.runAsync(argv, function(result)
    if result.exitCode ~= 0 then
      local why = firstLine(result.stderr)
      noctalia.notifyError("GitLab Trackr", why ~= "" and why or "tt open failed")
    end
  end)
end
