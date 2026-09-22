local config = assert(remuda._butler_mail_config)
local bus = assert(config.bus)
bus.mail_loaded = bus.mail_loaded or {}
bus.mail_read = bus.mail_read or {}

local function mailbox(name)
  bus.inboxes[name] = bus.inboxes[name] or {}
  return bus.inboxes[name]
end

local function address(session)
  return { host = "local", session = session }
end

local function file_component(value)
  return (value:gsub(".", function(byte)
    return string.format("%02x", string.byte(byte))
  end))
end

local function message_id()
  bus.next = bus.next + 1
  local seed = os.tmpname()
  os.remove(seed)
  local suffix = (seed:match("([^/]+)$") or seed):gsub("[^%w_-]", "-")
  return "message-" .. string.format("%x", os.time()) .. "-" .. string.format("%x", bus.next) .. "-" .. suffix
end

local function write_atomic(path, content)
  local temporary = path .. ".tmp-" .. message_id()
  local file, err = io.open(temporary, "w")
  if not file then return nil, err end
  local ok, write_err = file:write(content)
  local close_ok, close_err = file:close()
  if not ok or not close_ok then
    os.remove(temporary)
    return nil, write_err or close_err
  end
  local renamed, rename_err = os.rename(temporary, path)
  if not renamed then
    os.remove(temporary)
    return nil, rename_err
  end
  return true
end

local function append(path, content)
  local file, err = io.open(path, "a")
  if not file then return nil, err end
  local ok, write_err = file:write(content)
  local close_ok, close_err = file:close()
  if not ok or not close_ok then return nil, write_err or close_err end
  return true
end

local function paths(name)
  local root = config.root
  if not root then return nil end
  return {
    objects = root .. "/objects/", messages = root .. "/messages/",
    inbox = root .. "/inboxes/" .. file_component(name) .. ".jsonl",
    read = root .. "/read/" .. file_component(name) .. ".jsonl",
  }
end

local function prepare_storage()
  if not config.root then return true end
  local ok, err = pcall(function()
    remuda.mkdir(config.root .. "/objects")
    remuda.mkdir(config.root .. "/messages")
    remuda.mkdir(config.root .. "/inboxes")
    remuda.mkdir(config.root .. "/read")
  end)
  return ok, err
end

local function envelope_json(message, object)
  return '{"id":' .. config.json_quote(message.id) .. ',"from":{"host":'
    .. config.json_quote(message.from.host) .. ',"session":' .. config.json_quote(message.from.session)
    .. '},"to":[{"host":' .. config.json_quote(message.to[1].host) .. ',"session":'
    .. config.json_quote(message.to[1].session) .. '}],"subject":' .. config.json_quote(message.subject)
    .. ',"created_at":' .. config.json_quote(message.created_at) .. ',"content_type":'
    .. config.json_quote(message.content_type) .. ',"body":{"object_id":'
    .. config.json_quote(object.id) .. ',"bytes":' .. tostring(object.bytes) .. ',"content_type":'
    .. config.json_quote(object.content_type) .. ',"content_hash":null}}\n'
end

local function load_read(name)
  if bus.mail_read[name] then return bus.mail_read[name] end
  local read = {}
  local disk = paths(name)
  local file = disk and io.open(disk.read, "r")
  if file then
    for id in file:lines() do read[id] = true end
    file:close()
  end
  bus.mail_read[name] = read
  return read
end

local function load_message(disk, id)
  if bus.messages[id] then return bus.messages[id] end
  local envelope_file = io.open(disk.messages .. id .. ".json", "r")
  if not envelope_file then return nil end
  local envelope = envelope_file:read("*a")
  envelope_file:close()
  local object_id = envelope:match('"object_id":"([^"]+)"')
  if not object_id then return nil end
  local object_file = io.open(disk.objects .. object_id, "r")
  if not object_file then return nil end
  local content = object_file:read("*a")
  object_file:close()
  local sender = envelope:match('"session":"([^"]+)"') or "unknown"
  local subject = envelope:match('"subject":"([^"]*)"') or "Message"
  local created_at = envelope:match('"created_at":"([^"]+)"') or "unknown"
  local message = { id = id, from = address(sender), subject = subject, created_at = created_at,
    body = { object_id = object_id }, content_type = "text/plain; charset=utf-8" }
  bus.messages[id] = message
  bus.objects[object_id] = { id = object_id, content = content, bytes = #content,
    content_type = "text/plain; charset=utf-8" }
  return message
end

local function load_inbox(name)
  if bus.mail_loaded[name] then return end
  bus.mail_loaded[name] = true
  local disk = paths(name)
  if not disk then return end
  local read = load_read(name)
  local file = io.open(disk.inbox, "r")
  if not file then return end
  for line in file:lines() do
    local id = line:match('"message_id":"([^"]+)"')
    if id and not read[id] then
      load_message(disk, id)
      mailbox(name)[#mailbox(name) + 1] = id
    end
  end
  file:close()
end

local function queue(from, to, text, subject, in_reply_to)
  local id = message_id()
  local object_id = id:gsub("^message%-", "object-")
  local sender, body = from or "outside", tostring(text)
  local object = { id = object_id, content = body, bytes = #body,
    content_type = "text/plain; charset=utf-8", content_hash = nil }
  local message = { id = id, from = address(sender), to = { address(to) },
    subject = subject or ("Message from " .. sender), created_at = os.date("!%Y-%m-%dT%H:%M:%SZ"),
    in_reply_to = in_reply_to, content_type = "text/plain; charset=utf-8", body = { object_id = object_id } }
  local disk = paths(to)
  if disk then
    local ready, ready_err = prepare_storage()
    if not ready then return nil, "cannot prepare Butler mail storage: " .. tostring(ready_err) end
    local wrote, err = write_atomic(disk.objects .. object_id, body)
    if not wrote then return nil, "cannot write Butler mail body: " .. tostring(err) end
    wrote, err = write_atomic(disk.messages .. id .. ".json", envelope_json(message, object))
    if not wrote then return nil, "cannot write Butler mail envelope: " .. tostring(err) end
    wrote, err = append(disk.inbox, '{"message_id":' .. config.json_quote(id) .. '}\n')
    if not wrote then return nil, "cannot deliver Butler mail: " .. tostring(err) end
  end
  bus.objects[object_id], bus.messages[id] = object, message
  mailbox(to)[#mailbox(to) + 1] = id
  return message
end

local function inbox(name)
  load_inbox(name)
  local messages = mailbox(name)
  if #messages == 0 then return "inbox empty" end
  local out, read = {}, load_read(name)
  for _, id in ipairs(messages) do
    local message = bus.messages[id]
    local object = message and bus.objects[message.body.object_id]
    if message and object then
      out[#out + 1] = "[" .. message.id .. " from " .. message.from.host .. "/"
        .. message.from.session .. " · " .. message.created_at .. "] " .. message.subject .. "\n" .. object.content
      read[id] = true
    end
  end
  local disk = paths(name)
  if disk then
    local wrote, err = append(disk.read, table.concat(messages, "\n") .. "\n")
    if not wrote then return "mail read-state was not saved: " .. tostring(err) end
  end
  bus.inboxes[name] = {}
  return table.concat(out, "\n")
end

remuda._butler_mail = { mailbox = mailbox, queue = queue, inbox = inbox }
