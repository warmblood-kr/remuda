-- Built-in cluster UI composition.
--
-- This file is the default `compose(model)` implementation. It is embedded in
-- the binary and loaded in the restricted frontend VM. A user document at
-- Config/cluster-ui.lua may replace `compose`; it inherits these helpers.
--
-- The return value is a table (not JSON). Rust validates it atomically: every
-- model group and session must appear exactly once, each operational block
-- must carry exactly one `field = "status"` span, and the layout must fit.
-- Rust substitutes the canonical status text; this file chooses position,
-- labels, order, colors and the split. It never owns status truth.

local COLORS = {
  cyan = { index = 6 },
  green = { index = 2 },
  red = { index = 1 },
}

local function span(text, style)
  local out = { text = text }
  if style then
    for key, value in pairs(style) do
      out[key] = value
    end
  end
  return out
end

local function plain(text)
  return span(text)
end

local function status(style)
  local out = { field = "status" }
  if style then
    for key, value in pairs(style) do
      out[key] = value
    end
  end
  return out
end

local function width(text)
  return cell_width(text)
end

local function pad(text, cells)
  local missing = cells - width(text)
  if missing <= 0 then
    return text
  end
  return text .. string.rep(" ", missing)
end

local function selected_style(surface, selected)
  if surface == "peer" then
    -- The peer list kept plain names with reverse video for the selection;
    -- no caret gutter and no color.
    return selected and { reverse = true } or nil
  end
  if selected then
    return { bold = true, reverse = true, fg = COLORS.cyan }
  end
  return { bold = true }
end

local function session_status_style(alive)
  if alive then
    return { fg = COLORS.green }
  end
  return { fg = COLORS.red }
end

-- A frontend decorator may add presentation to the approved facts. Rust still
-- validates exact targets, canonical status slots, bounds and geometry.
function decorate_session(session, spans)
  local facts = session.display
  if not facts then return spans end
  local parts = {}
  if facts.leader then parts[#parts + 1] = "← " .. facts.leader end
  if facts.kind then parts[#parts + 1] = facts.kind end
  if facts.model then parts[#parts + 1] = facts.model end
  if facts.context_percent then parts[#parts + 1] = tostring(facts.context_percent) .. "% ctx" end
  if #parts > 0 then spans[#spans + 1] = plain(" · " .. table.concat(parts, " · ")) end
  return spans
end

local function session_spans(surface, session)
  local label = span(session.label, selected_style(surface, session.selected))
  local decorated = decorate_session(session, { label })
  if surface == "cluster" then
    -- Four spaces after Rust's one-cell cursor gutter match the old tree's
    -- five-column session prefix; leave the name aligned at column 12.
    local spans = { plain("    ") }
    for _, item in ipairs(decorated) do spans[#spans + 1] = item end
    spans[#spans + 1] = plain(pad("", 12 - width(session.label)))
    spans[#spans + 1] = plain(" ")
    spans[#spans + 1] = status(session_status_style(session.alive))
    return spans
  end
  -- The peer list keeps its baseline plain name and reverse selection; the
  -- canonical status slot is structurally present but its text is empty.
  decorated[#decorated + 1] = status()
  return decorated
end

local function group_spans(group)
  local arrow = group.expanded and "▼" or "▶"
  if group.kind == "node" then
    return {
      plain(arrow .. " "),
      span(group.label, selected_style("cluster", group.selected)),
      plain(" "),
      status(),
    }
  end
  -- Local node rows were plain text: the local node is this machine, so its
  -- reachability needs no Rust-appended status.
  local age = group.age_seconds or 0
  return { plain(arrow .. " " .. group.label .. "  local · reachable · sync " .. age .. "s") }
end

local function make_row(spans)
  return { spans = spans }
end

function compose(model)
  local surface = model.surface or "cluster"
  local viewport = model.viewport or {}
  local gutter = viewport.gutter_cells or 1
  local max_cells = 0

  local function measure(spans, gutter_cells, status_cells)
    local cells = gutter_cells or 0
    for _, item in ipairs(spans) do
      if item.field == "status" then
        cells = cells + (status_cells or 0)
      else
        cells = cells + width(item.text or "")
      end
    end
    if cells > max_cells then
      max_cells = cells
    end
  end

  local function row(spans, has_gutter, status_cells)
    measure(spans, has_gutter and gutter or 0, status_cells)
    return make_row(spans)
  end

  -- Paint width can differ from measured width: the baseline peer empty hint
  -- is measured unindented and indented only at paint time.
  local function measured_row(spans, cells)
    if cells > max_cells then
      max_cells = cells
    end
    return make_row(spans)
  end

  local title = {}
  if surface == "cluster" then
    local counts = model.counts or { reachable = 1, total = 1 }
    title[#title + 1] = row({
      plain("remuda · cluster (" .. counts.reachable .. "/" .. counts.total .. " reachable)"),
    }, false)
  end

  local groups = {}
  local local_empty = false
  for _, group in ipairs(model.groups) do
    local rows = {}
    if group.kind == "node" then
      rows[#rows + 1] = row(group_spans(group), true, group.status_cells)
    elseif group.kind == "local" then
      rows[#rows + 1] = row(group_spans(group), false, group.status_cells)
      local_empty = group.expanded and #group.sessions == 0
    end
    local sessions = {}
    for _, session in ipairs(group.sessions) do
      sessions[#sessions + 1] = {
        id = session.id,
        rows = { row(session_spans(surface, session), gutter > 0, session.status_cells) },
      }
    end
    groups[#groups + 1] = { id = group.id, rows = rows, sessions = sessions }
  end

  local empty = {}
  if surface == "cluster" and local_empty then
    local hint
    if model.attention_only then
      hint = "    no sessions need attention"
    else
      hint = "    no matching sessions"
    end
    empty[#empty + 1] = row({ plain(hint) }, false)
    if model.all_empty then
      empty[#empty + 1] = row({ plain("    No sessions yet. Start one on any machine:") }, false)
      empty[#empty + 1] = row({ plain("    remuda run -n NAME COMMAND") }, false)
    end
  elseif surface == "peer" then
    local any = false
    for _, group in ipairs(model.groups) do
      if #group.sessions > 0 then
        any = true
      end
    end
    if not any then
      -- Baseline peer empty state: a blank first row, then a two-space hint
      -- whose content width is measured without the indent.
      empty[#empty + 1] = row({ plain("") }, false)
      empty[#empty + 1] = measured_row(
        { plain("  no sessions on this node") },
        cell_width("no sessions on this node")
      )
    end
  end

  local layout
  if surface == "cluster" then
    local cols = viewport.cols or 1
    if cols >= 60 then
      local limit = math.min(48, math.floor(cols / 2), cols - 2)
      local list_cells = math.max(1, math.min(max_cells + 1, limit))
      list_cells = math.max(math.min(24, limit), list_cells)
      layout = { axis = "horizontal", list_cells = list_cells, divider = "│" }
    else
      local body = viewport.body_rows or 1
      local rows_total = #title
      for _, group in ipairs(groups) do
        rows_total = rows_total + #group.rows
        for _, session in ipairs(group.sessions) do
          rows_total = rows_total + #session.rows
        end
      end
      rows_total = rows_total + #empty
      local list_cells = math.max(1, math.min(rows_total, body - 3))
      layout = { axis = "vertical", list_cells = list_cells, divider = "─" }
    end
  else
    local cols = viewport.cols or 1
    -- Baseline peer policy: usable = cols-1, max = (usable-16).max(16).min(usable).
    local limit = math.min(40, cols - 1, math.max(16, cols - 17))
    local wanted = math.max(16, max_cells + 2)
    local list_cells
    if model.preferred_list_cells and model.preferred_list_cells > 0 then
      list_cells = math.max(16, model.preferred_list_cells)
    else
      list_cells = wanted
    end
    list_cells = math.min(list_cells, limit)
    layout = { axis = "horizontal", list_cells = math.max(1, list_cells), divider = "│" }
  end

  return {
    version = 1,
    title = title,
    groups = groups,
    empty = empty,
    layout = layout,
  }
end
