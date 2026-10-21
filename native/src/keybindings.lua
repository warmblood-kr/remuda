-- One declaration authority; native clients consume immutable effective rows.
-- The factory's dependencies are captured before user code can replace them.
return function(owner, validate, config_status, validate_help)
  local entries, generation = {}, 0
  local api = {}
  local initializing = true
  local function copy(row)
    local out = {}
    for key, value in pairs(row) do out[key] = value end
    return out
  end
  local function effective(source)
    local actions = {}
    for _, row in ipairs(source) do
      local id = row.context .. '/' .. row.action
      local previous = actions[id]
      if not previous or row.owner == nil
        or (previous.owner ~= nil and row.owner ~= '__defaults') then actions[id] = row end
    end
    local rows, keys = {}, {}
    for _, row in pairs(actions) do
      if row.key then
        local slot = row.context .. '/' .. row.key
        if keys[slot] then error('conflicting binding: ' .. slot, 3) end
        keys[slot] = true
        rows[#rows + 1] = copy(row)
      end
    end
    table.sort(rows, function(a, b)
      if a.context ~= b.context then return a.context < b.context end
      return a.action < b.action
    end)
    return rows
  end
  local function find(context, action, key)
    for _, row in ipairs(effective(entries)) do
      if row.context == context and (row.action == action or row.key == key) then return row end
    end
  end
  local function commit(row)
    local next_entries = {}
    for _, old in ipairs(entries) do
      if not (old.owner == row.owner and old.context == row.context and old.action == row.action) then
        next_entries[#next_entries + 1] = old
      end
    end
    next_entries[#next_entries + 1] = row
    effective(next_entries) -- validate before changing either rows or generation
    entries, generation = next_entries, generation + 1
  end
  local function editable(row)
    local who = owner()
    if who and row.owner and row.owner ~= '__defaults' and row.owner ~= who then
      error('binding is owned by ' .. row.owner, 3)
    end
  end
  function api.define_key(spec)
    if type(spec) ~= 'table' then error('define_key needs a table', 2) end
    if type(spec.action) ~= 'string' or spec.action == '' then error('binding needs an action name', 2) end
    local key = validate(spec.context, spec.key, spec.action)
    if type(spec.help) ~= 'string' then error('binding needs printable help', 2) end
    validate_help(spec.help)
    if find(spec.context, spec.action, key) then error('conflicting key or action binding', 2) end
    commit({context=spec.context, key=key, action=spec.action, help=spec.help, owner=initializing and '__defaults' or owner()})
  end
  function api.rebind(context, action, key)
    if type(action) ~= 'string' or action == '' then error('binding needs an action name', 2) end
    key = validate(context, key, action)
    local row = find(context, action)
    if not row then error('no binding for action ' .. action, 2) end
    editable(row)
    local next_row = copy(row)
    next_row.key, next_row.owner = key, owner()
    commit(next_row)
  end
  function api.unbind(context, key)
    local row = find(context, nil, validate(context, key, nil))
    if not row then error('no binding for key ' .. key, 2) end
    editable(row)
    local next_row = copy(row)
    next_row.key, next_row.owner = nil, owner()
    commit(next_row)
  end
  function api.keymap_snapshot()
    local ready, diagnostic = config_status()
    local rows = effective(entries)
    for _, row in ipairs(rows) do row.owner = nil end
    return {generation=generation, ready=ready, diagnostic=diagnostic, rows=rows}
  end
  local function drop(who)
    local kept = {}
    for _, row in ipairs(entries) do if row.owner ~= who then kept[#kept + 1] = row end end
    effective(kept)
    entries, generation = kept, generation + 1
  end
  local function save(who)
    local saved = {}
    for index, row in ipairs(entries) do
      if row.owner == who then saved[#saved + 1] = {index=index, row=row} end
    end
    return saved
  end
  local function restore(who, saved)
    drop(who)
    for _, entry in ipairs(saved) do table.insert(entries, math.min(entry.index, #entries + 1), entry.row) end
    effective(entries)
    generation = generation + 1
  end
  -- Defaults appear nowhere in Rust or footer code.
  api.define_key{context='session.list.browse', key='l', action='session.toggle_list', help='list'}
  api.define_key{context='session.list.browse', key='m', action='session.compose', help='message'}
  api.define_key{context="cluster.tree", key="<up>", action="cluster.up", help="up"}
  api.define_key{context="cluster.tree", key="<down>", action="cluster.down", help="down"}
  api.define_key{context="cluster.tree", key="<left>", action="cluster.left", help="collapse"}
  api.define_key{context="cluster.tree", key="<right>", action="cluster.right", help="expand"}
  api.define_key{context="cluster.tree", key="RET", action="cluster.select", help="select"}
  api.define_key{context="cluster.search", key="<up>", action="cluster.up", help="up"}
  api.define_key{context="cluster.search", key="<down>", action="cluster.down", help="down"}
  api.define_key{context="cluster.search", key="<left>", action="cluster.left", help="collapse"}
  api.define_key{context="cluster.search", key="<right>", action="cluster.right", help="expand"}
  api.define_key{context="cluster.search", key="RET", action="cluster.select", help="select"}
  api.define_key{context="cluster.tree", key="q", action="cluster.detach", help="detach"}
  api.define_key{context="cluster.tree", key="x", action="cluster.close", help="close"}
  api.define_key{context="cluster.tree", key="/", action="cluster.search", help="search"}
  api.define_key{context="cluster.tree", key="!", action="cluster.attention", help="attention"}
  api.define_key{context="cluster.tree", key="k", action="cluster.remote.keys", help="keys"}
  api.define_key{context="cluster.search", key="ESC", action="cluster.search.clear", help="clear"}
  api.define_key{context="cluster.search", key="DEL", action="cluster.search.delete", help="delete"}
  api.define_key{context="cluster.composer", key="RET", action="cluster.composer.submit", help="send"}
  api.define_key{context="cluster.composer", key="C-c", action="cluster.composer.clear", help="clear"}
  api.define_key{context="cluster.composer", key="ESC", action="cluster.composer.back", help="list"}
  api.define_key{context="cluster.composer", key="C-\\", action="cluster.composer.detach", help="list"}
  api.define_key{context="cluster.composer", key="<left>", action="cluster.composer.left", help="left"}
  api.define_key{context="cluster.composer", key="<right>", action="cluster.composer.right", help="right"}
  api.define_key{context="cluster.composer", key="<up>", action="cluster.composer.up", help="up"}
  api.define_key{context="cluster.composer", key="<down>", action="cluster.composer.down", help="down"}
  api.define_key{context="cluster.composer", key="C-f", action="cluster.composer.forward", help="forward"}
  api.define_key{context="cluster.composer", key="C-b", action="cluster.composer.backward", help="backward"}
  api.define_key{context="cluster.composer", key="<home>", action="cluster.composer.home", help="home"}
  api.define_key{context="cluster.composer", key="<end>", action="cluster.composer.end", help="end"}
  api.define_key{context="cluster.composer", key="DEL", action="cluster.composer.backspace", help="erase"}
  api.define_key{context="cluster.composer", key="<delete>", action="cluster.composer.delete", help="delete"}
  api.define_key{context="cluster.composer", key="C-a", action="cluster.composer.home_emacs", help="start"}
  api.define_key{context="cluster.composer", key="C-e", action="cluster.composer.end_emacs", help="end"}
  api.define_key{context="cluster.composer", key="C-k", action="cluster.composer.kill", help="kill tail"}
  api.define_key{context="cluster.confirm", key="y", action="cluster.confirm.accept", help="confirm"}
  api.define_key{context="cluster.confirm", key="Y", action="cluster.confirm.accept_upper", help="confirm"}
  api.define_key{context="cluster.remote.keys", key="C-\\", action="cluster.remote.back", help="back"}
  api.define_key{context="cluster.runtime", key="C-l", action="cluster.redraw", help="redraw"}
  initializing = false
  return api, {drop=drop, save=save, restore=restore}
end
