-- worktree-sync:create
-- Makes one call that creates blocks (`method` with `args`, in `session_id`
-- when given) and prints its reply. The calling client owns the blocks it
-- creates, and a block follows its owner's size, so a Rex app would draw them
-- only once it takes them over, seconds after this client exits. Giving them
-- up hands them to the app at once.
local a = rex.args
local function must(result, err)
  if result == nil then error(err, 0) end
  return result
end

must(rex.call("session.attach_all", { event_scope = "session" }))
local args = a.args or {}
if a.session_id then
  -- Giving blocks up needs the session attached.
  must(rex.call("session.attach", { session_id = a.session_id }))
  args.session_id = a.session_id
end
local reply = must(rex.call(a.method, args))

local session_id = a.session_id or reply.session_id
local blocks = {}
for _, id in ipairs(reply.block_ids or {}) do blocks[#blocks + 1] = id end
for _, w in ipairs(reply.initial_windows or {}) do
  for _, id in ipairs(w.block_ids or {}) do blocks[#blocks + 1] = id end
end
-- Best effort: a block kept is still usable, only drawn late.
if session_id and #blocks > 0
  and (a.session_id or rex.call("session.attach", { session_id = session_id })) then
  rex.call("session.disown_block", { session_id = session_id, block_ids = blocks })
end
print(rex.json.encode(reply))
