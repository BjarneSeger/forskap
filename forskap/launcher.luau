-- Noctalia launcher provider for forskapd: `/gl <query>`.
--
-- Thin by design, like `forskap` itself: every query is one `forskap search --output
-- json` run, every activation one `forskap issue|mr|epic open` (issues/MRs/epics — which
-- also records the open so the daemon ranks it higher next time) or `xdg-open`
-- (projects/groups). The daemon owns ranking; results are emitted in its
-- order with `score = open_count`, so frequently opened issues, MRs and epics
-- interleave across kinds instead of one kind always sitting on top.
--
-- Plain text searches projects: the launcher's job is mostly "open that repo",
-- and the issues and MRs would bury the project rows. A leading kind word or
-- sigil picks another kind, `all` every kind; `/gl` alone is the frequently
-- opened view across kinds. The grammar is mirrored in the GNOME/KRunner
-- provider (`forskap-cli/src/cmd/integration/search_provider/query.rs`).

local CATEGORY = {
  issues = "Issues",
  merge_requests = "Merge requests",
  epics = "Epics",
  projects = "Projects",
  groups = "Groups",
}

local GLYPH = {
  issues = "circle-dot",
  merge_requests = "git-pull-request",
  epics = "flag",
  projects = "folder",
  groups = "users",
}

-- Leading word → `forskap search --kind` value.
local KIND_WORDS = {
  i = "issues", issue = "issues", issues = "issues",
  mr = "merge_requests", mrs = "merge_requests",
  p = "projects", project = "projects", projects = "projects",
  g = "groups", group = "groups", groups = "groups",
  e = "epics", epic = "epics", epics = "epics",
}

local function trim(s)
  return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end

-- Split the text behind the prefix into the kind filter (nil: every kind), the
-- query handed to `forskap search` and whether the kind is the default rather
-- than typed. Pure, so the shapes are easy to eyeball:
--   "oauth"        → "projects",       "oauth"
--   ""             → nil,              ""        (frequently opened issues/MRs/epics)
--   "all oauth"    → nil,              "oauth"   (`*` works too)
--   "mr oauth"     → "merge_requests", "oauth"
--   "mr"           → "merge_requests", ""        (frequently opened MRs)
--   "!42"          → "merge_requests", "#42"     (`forskap` only knows the # form)
--   "#42"          → "issues",         "#42"
--   "&42"          → "epics",          "&42"
local function parseQuery(text)
  local t = trim(text or "")
  local word, rest = t:match("^(%S+)%s*(.*)$")
  if word ~= nil then
    local lower = word:lower()
    if lower == "all" or word == "*" then
      return nil, trim(rest)
    end
    if KIND_WORDS[lower] ~= nil then
      return KIND_WORDS[lower], trim(rest)
    end
  end
  local mrNumber = t:match("^!(%d+)$")
  if mrNumber ~= nil then
    return "merge_requests", "#" .. mrNumber
  end
  if t:match("^#%d+$") then
    return "issues", t
  end
  if t:match("^&%d+$") then
    return "epics", t
  end
  if t == "" then
    return nil, "", true
  end
  return "projects", t, true
end

-- The configured path wins; otherwise `forskap` from noctalia's PATH, falling back
-- to the usual cargo/user bin dirs a login shell has but the compositor
-- session often lacks.
local function forskapPath()
  local configured = noctalia.getConfig("forskap_path")
  if configured ~= nil and configured ~= "" and configured ~= "forskap" then
    return noctalia.expandPath(configured)
  end
  if noctalia.commandExists("forskap") then
    return "forskap"
  end
  for _, dir in ipairs({ "~/.cargo/bin/", "~/.local/bin/" }) do
    local candidate = noctalia.expandPath(dir .. "forskap")
    if noctalia.fileExists(candidate) then
      return candidate
    end
  end
  return "forskap"
end

-- "https://gl/team/api/-/issues/42" → "team/api"
local function projectPath(webUrl)
  local path = (webUrl or ""):match("^https?://[^/]+/(.-)/%-/")
  return path or ""
end

-- The path the item carries (`project_path` from an older `forskap`), else
-- the one in its URL.
local function issuableProject(item)
  local path = item.namespace_path or item.project_path
  if path ~= nil and path ~= "" then
    return path
  end
  return projectPath(item.web_url)
end

-- "https://gl/groups/team/-/epics/5" → "team": group pages live under
-- `/groups/`, which is no part of the group's path.
local function groupPath(webUrl)
  return (projectPath(webUrl):gsub("^groups/", ""))
end

-- The project's avatar as the daemon downloaded it, nil when there is none.
-- noctalia takes an absolute path for `icon` and shows `glyph` until the
-- image is loaded, but swaps a missing file for a generic icon: the avatars
-- are cache files, so check.
local function avatarIcon(path)
  if path ~= nil and path ~= "" and noctalia.fileExists(path) then
    return path
  end
  return nil
end

-- The seconds spent on an issue as `forskap` shows them ("1h 30m", "45s"),
-- nil when none is logged. An older `forskap` sends GitLab's own spelling as
-- `total_time`.
local function timeSpent(item)
  local secs = item.time_spent
  if secs == nil then
    return item.total_time
  end
  if secs <= 0 then
    return nil
  end
  local hours, mins = math.floor(secs / 3600), math.floor(secs % 3600 / 60)
  if hours == 0 and mins == 0 then
    return string.format("%ds", secs)
  end
  local parts = {}
  if hours > 0 then
    table.insert(parts, string.format("%dh", hours))
  end
  if mins > 0 then
    table.insert(parts, string.format("%dm", mins))
  end
  return table.concat(parts, " ")
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
    subtitle = joinNonEmpty({ issuableProject(item), item.state, timeSpent(item) }),
    glyph = GLYPH[kind],
    icon = avatarIcon(item.project_avatar),
    category = CATEGORY[kind],
    score = item.open_count or 0,
  }
end

-- The group path the epic carries (`group_path` from an older `forskap`),
-- else the one in its URL.
local function epicGroup(item)
  local path = item.namespace_path or item.group_path
  if path ~= nil and path ~= "" then
    return path
  end
  return groupPath(item.web_url)
end

-- Epics belong to a group: the id carries the group where an issue's carries
-- the project.
local function epicRow(item)
  return {
    id = string.format("epic:%d:%d", item.group_id, item.iid),
    title = string.format("&%d %s", item.iid, item.title),
    subtitle = joinNonEmpty({ epicGroup(item), item.state }),
    glyph = GLYPH.epics,
    category = CATEGORY.epics,
    score = item.open_count or 0,
  }
end

-- `path` from an older `forskap`.
local function urlRow(kind, item)
  return {
    id = "url:" .. (item.web_url or ""),
    title = item.full_path or item.path,
    subtitle = item.name,
    glyph = GLYPH[kind],
    icon = avatarIcon(item.avatar),
    category = CATEGORY[kind],
  }
end

-- Issues and epics are both work items, told apart by their type.
local function isEpic(item)
  return (item.type or ""):lower() == "epic"
end

local function toResults(reply)
  local results = {}
  -- An older `forskap` sends `issues` and `epics` instead of `work_items`.
  local issues, epics = {}, {}
  for _, w in ipairs(reply.work_items or {}) do
    table.insert(isEpic(w) and epics or issues, w)
  end
  for _, i in ipairs(reply.issues or {}) do
    table.insert(issues, i)
  end
  for _, e in ipairs(reply.epics or {}) do
    table.insert(epics, e)
  end
  for _, i in ipairs(issues) do
    table.insert(results, issuableRow("issues", "#", i))
  end
  for _, m in ipairs(reply.merge_requests or {}) do
    table.insert(results, issuableRow("merge_requests", "!", m))
  end
  for _, e in ipairs(epics) do
    table.insert(results, epicRow(e))
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
  -- `forskap` prefixes anyhow errors with "Error: ".
  return trim((line or ""):gsub("^Error:%s*", ""))
end

local function messageRow(title, glyph)
  return { { id = "message", title = title, glyph = glyph } }
end

function onQuery(text)
  local kind, query, defaulted = parseQuery(text)
  local argv = { forskapPath(), "search", "--output", "json" }
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
        why = "forskap exited with " .. tostring(result.exitCode)
      end
      launcher.setResults(text, messageRow(why, "alert-circle"))
      return
    end
    local reply, err = noctalia.json.decode(result.stdout)
    if reply == nil then
      launcher.setResults(text, messageRow("Unreadable forskap output: " .. tostring(err), "alert-circle"))
      return
    end
    local results = toResults(reply)
    if #results == 0 then
      local hint = "No matches"
      if query == "" then
        hint = "Nothing opened yet — search, then open something"
      elseif defaulted then
        hint = "No matching projects — i, mr, e, g pick another kind, all searches everything"
      end
      results = messageRow(hint, "search")
    end
    launcher.setResults(text, results)
  end)
  if not started then
    launcher.setResults(text, messageRow("Could not start forskap — check the forskap binary setting", "alert-circle"))
  end
end

function onActivate(id)
  if id == "message" then
    return
  end
  local url = id:match("^url:(.+)$")
  if url ~= nil then
    if not noctalia.runAsync({ "xdg-open", url }) then
      noctalia.notifyError("Forskap", "Could not open " .. url)
    end
    return
  end
  -- `ownerId` is the project of an issue or MR, the group of an epic.
  local kind, ownerId, iid = id:match("^([%a_]+):(%d+):(%d+)$")
  if kind == nil then
    return
  end
  local group = kind == "merge_requests" and "mr" or kind == "epic" and "epic" or "issue"
  local argv = { forskapPath(), group, "open", iid, group == "epic" and "-g" or "-p", ownerId }
  noctalia.runAsync(argv, function(result)
    if result.exitCode ~= 0 then
      local why = firstLine(result.stderr)
      noctalia.notifyError("Forskap", why ~= "" and why or "forskap " .. group .. " open failed")
    end
  end)
end
