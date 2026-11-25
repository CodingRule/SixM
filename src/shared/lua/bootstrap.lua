-- SixM Lua bootstrap.
-- Loaded once into every LuaRuntime state, before any resource script.
-- Receives the JS bridge table as its only argument and returns the table of
-- internal entry points the JS side calls (tick, trigger, command, ...).
-- Neither table is ever stored in a global, so resource scripts cannot reach them.

local bridge = ...

-- Capture everything we rely on as locals so a script that overwrites a
-- global (coroutine, table, ...) cannot break the scheduler.
local G = _G
local type, tostring, tonumber, select, error = type, tostring, tonumber, select, error
local setmetatable, getmetatable, rawget, rawset, rawequal = setmetatable, getmetatable, rawget, rawset, rawequal
local co_create, co_resume, co_yield = coroutine.create, coroutine.resume, coroutine.yield
local co_status, co_isyieldable = coroutine.status, coroutine.isyieldable
local traceback = debug.traceback
local concat, remove, pack = table.concat, table.remove, table.pack
local sqrt, format = math.sqrt, string.format
local rawload = load

local internal = {}

-------------------------------------------------------------------------------
-- Sandbox (the JS side already opens only safe libraries; this trims the rest)
-------------------------------------------------------------------------------

dofile = nil
loadfile = nil

-- Text chunks only: precompiled bytecode is refused.
function load(chunk, chunkname, _, ...)
  if select('#', ...) == 0 then
    return rawload(chunk, chunkname, 't')
  end
  return rawload(chunk, chunkname, 't', (...))
end

function require(name)
  error(("require(%q) is not available: SixM resources cannot load modules from disk"):format(tostring(name)), 2)
end

function print(...)
  local parts = pack(...)
  for i = 1, parts.n do parts[i] = tostring(parts[i]) end
  bridge.print(concat(parts, '\t', 1, parts.n))
end

-------------------------------------------------------------------------------
-- Scheduler. Sleeping coroutines are grouped into FIFO buckets by wake-up
-- deadline, and a binary min-heap orders the distinct deadlines. Threads that
-- wait from the same tick share a deadline, so the common Wait() is one table
-- lookup and an append, and waking a whole bucket costs a single heap pop.
-------------------------------------------------------------------------------

local buckets = {}        -- deadline -> { n = count, [1..n] = coroutine }
local heap, heapN = {}, 0 -- min-heap of the deadlines that currently have a bucket
local sleeping = 0        -- total coroutines across all buckets
local tickNow = nil       -- the `now` of the tick in progress, if any
local threads = {}        -- live coroutine -> { context = string, source = any }

local function sleep(co, ms)
  ms = tonumber(ms) or 0
  if not (ms >= 0) then ms = 0 end -- also catches NaN
  local deadline = (tickNow or bridge.now()) + ms
  sleeping = sleeping + 1
  local bucket = buckets[deadline]
  if bucket then
    local n = bucket.n + 1
    bucket.n = n
    bucket[n] = co
    return
  end
  buckets[deadline] = { co, n = 1 }
  -- New deadline: sift it up from the end of the heap.
  local i = heapN + 1
  heapN = i
  while i > 1 do
    local parent = i // 2
    local parentDeadline = heap[parent]
    if parentDeadline <= deadline then break end
    heap[i] = parentDeadline
    i = parent
  end
  heap[i] = deadline
end

-- Removes the earliest deadline from the heap and returns its bucket.
local function popBucket()
  local deadline = heap[1]
  local n = heapN
  local last = heap[n]
  heap[n] = nil
  n = n - 1
  heapN = n
  if n > 0 then
    -- Sift the former last entry down from the root.
    local i = 1
    while true do
      local child = i * 2
      if child > n then break end
      local childDeadline = heap[child]
      local right = child + 1
      if right <= n and heap[right] < childDeadline then
        child, childDeadline = right, heap[right]
      end
      if last <= childDeadline then break end
      heap[i] = childDeadline
      i = child
    end
    heap[i] = last
  end
  local bucket = buckets[deadline]
  buckets[deadline] = nil
  sleeping = sleeping - bucket.n
  return bucket
end

-- Handles the outcome of a coroutine.resume: returns `false, traceback` when
-- the coroutine died with an error, `true` when it went back to sleep, and
-- `true, results...` when it finished.
local function settle(co, ok, ...)
  if not ok then
    threads[co] = nil
    return false, traceback(co, tostring((...)))
  end
  if co_status(co) == 'suspended' then
    sleep(co, (...))
    return true
  end
  threads[co] = nil
  return true, ...
end

local function restoreSource(previous, ...)
  rawset(G, 'source', previous)
  return ...
end

-- Resumes a managed coroutine with its own `source` global in place.
local function resume(co, ...)
  local previous = rawget(G, 'source')
  rawset(G, 'source', threads[co].source)
  return restoreSource(previous, settle(co, co_resume(co, ...)))
end

-- Resume and report: used wherever nobody is waiting for the result.
local function run(co, ...)
  local context = threads[co].context
  local ok, err = resume(co, ...)
  if not ok then bridge.report(err, context) end
end

-- Starts `fn` in a new managed coroutine, running it until its first Wait.
local function spawn(fn, context, source, ...)
  local co = co_create(fn)
  threads[co] = { context = context, source = source }
  run(co, ...)
end

local function raiseOnFailure(ok, ...)
  if not ok then error((...), 0) end
  return ...
end

-- Like spawn, but for callers that want the result: returns the function's
-- results if it finished without waiting, raises (with traceback) if it
-- failed, and returns nothing if it is still sleeping in a Wait.
local function invoke(fn, context, ...)
  local co = co_create(fn)
  threads[co] = { context = context, source = rawget(G, 'source') }
  return raiseOnFailure(resume(co, ...))
end

local function checkFunction(value, what, level)
  if type(value) ~= 'function' then
    error(what .. ' expects a function, got ' .. type(value), level + 1)
  end
end

local function checkName(value, what, level)
  if type(value) ~= 'string' then
    error(what .. ' expects a string name, got ' .. type(value), level + 1)
  end
end

function CreateThread(fn)
  checkFunction(fn, 'CreateThread', 2)
  spawn(fn, 'thread', rawget(G, 'source'))
end

function Wait(ms)
  if not co_isyieldable() then
    error('Wait() can only be called inside a thread (CreateThread), event handler, command, export or timeout', 2)
  end
  co_yield(ms)
end

function SetTimeout(ms, fn)
  checkFunction(fn, 'SetTimeout', 2)
  local co = co_create(fn)
  threads[co] = { context = 'timeout', source = rawget(G, 'source') }
  sleep(co, ms)
end

Citizen = { CreateThread = CreateThread, Wait = Wait, SetTimeout = SetTimeout }

function internal.tick(now)
  if heapN == 0 or heap[1] > now then return end
  -- Detach every due bucket first, run second: a thread that calls Wait(0)
  -- while running lands in a fresh bucket and is picked up by the *next* tick.
  local due, dueN = {}, 0
  while heapN > 0 and heap[1] <= now do
    dueN = dueN + 1
    due[dueN] = popBucket()
  end
  local previousNow, outerSource = tickNow, rawget(G, 'source')
  tickNow = now
  -- Same outcome handling as resume/settle/run, written out flat: every Lua
  -- call saved here is paid back once per sleeping thread per wake-up.
  for b = 1, dueN do
    local bucket = due[b]
    for i = 1, bucket.n do
      local co = bucket[i]
      local info = threads[co]
      rawset(G, 'source', info.source)
      local ok, value = co_resume(co)
      if not ok then
        threads[co] = nil
        bridge.report(traceback(co, tostring(value)), info.context)
      elseif co_status(co) == 'suspended' then
        sleep(co, value)
      else
        threads[co] = nil
      end
    end
  end
  rawset(G, 'source', outerSource)
  tickNow = previousNow
end

function internal.pending()
  return sleeping
end

function internal.invoke(fn, context, ...)
  return invoke(fn, context, ...)
end

-------------------------------------------------------------------------------
-- Events
-------------------------------------------------------------------------------

local handlers = {}    -- event name -> ordered list of { key = n, fn = function }
local netEvents = {}   -- event name -> true once allowed by RegisterNetEvent
local nextHandlerKey = 0

function AddEventHandler(name, fn)
  checkName(name, 'AddEventHandler', 2)
  checkFunction(fn, 'AddEventHandler', 2)
  nextHandlerKey = nextHandlerKey + 1
  local list = handlers[name]
  if not list then
    list = {}
    handlers[name] = list
  end
  list[#list + 1] = { key = nextHandlerKey, fn = fn }
  return { name = name, key = nextHandlerKey }
end

function RemoveEventHandler(handle)
  if type(handle) ~= 'table' or type(handle.name) ~= 'string' then
    error('RemoveEventHandler expects a handle returned by AddEventHandler', 2)
  end
  local list = handlers[handle.name]
  if not list then return end
  for i = 1, #list do
    if list[i].key == handle.key then
      list[i].removed = true -- seen by a dispatch already iterating a snapshot
      remove(list, i)
      break
    end
  end
  if #list == 0 then handlers[handle.name] = nil end
end

function RegisterNetEvent(name, fn)
  checkName(name, 'RegisterNetEvent', 2)
  netEvents[name] = true
  if fn ~= nil then
    checkFunction(fn, 'RegisterNetEvent', 2)
    return AddEventHandler(name, fn)
  end
end

function TriggerEvent(name, ...)
  checkName(name, 'TriggerEvent', 2)
  bridge.triggerEvent(name, ...)
end

function internal.trigger(name, source, fromNet, ...)
  if fromNet and not netEvents[name] then return 0 end
  local list = handlers[name]
  if not list then return 0 end
  -- Snapshot so handlers may add/remove handlers while we dispatch.
  local n = #list
  local snapshot = {}
  for i = 1, n do snapshot[i] = list[i] end
  local invoked = 0
  local context = 'event:' .. name
  for i = 1, n do
    local entry = snapshot[i]
    if not entry.removed then
      invoked = invoked + 1
      spawn(entry.fn, context, source, ...)
    end
  end
  return invoked
end

-------------------------------------------------------------------------------
-- Commands
-------------------------------------------------------------------------------

local commands = {}    -- command name -> { fn = function, restricted = boolean }

function RegisterCommand(name, fn, restricted)
  checkName(name, 'RegisterCommand', 2)
  checkFunction(fn, 'RegisterCommand', 2)
  commands[name] = { fn = fn, restricted = restricted and true or false }
end

function internal.command(name, source, args, raw)
  local command = commands[name]
  if not command then return false end
  spawn(command.fn, 'command:' .. name, source, source, args or {}, raw)
  return true
end

function internal.hasCommand(name)
  return commands[name] ~= nil
end

function internal.isCommandRestricted(name)
  local command = commands[name]
  return command ~= nil and command.restricted
end

-------------------------------------------------------------------------------
-- Exports: `exports('name', fn)` publishes, `exports.res:fn(...)` calls.
-------------------------------------------------------------------------------

local published = {}   -- export name -> function
local proxies = {}     -- resource name -> proxy table

local function exportProxy(resource)
  local proxy = proxies[resource]
  if proxy then return proxy end
  proxy = setmetatable({}, {
    __index = function(self, exportName)
      return function(...)
        -- Accept both exports.res:fn(a) and exports.res.fn(a).
        if select('#', ...) > 0 and rawequal((...), self) then
          return bridge.callExport(resource, exportName, select(2, ...))
        end
        return bridge.callExport(resource, exportName, ...)
      end
    end,
    __newindex = function()
      error("exports of another resource are read-only; publish with exports('name', fn)", 2)
    end,
  })
  proxies[resource] = proxy
  return proxy
end

exports = setmetatable({}, {
  __call = function(_, name, fn)
    checkName(name, 'exports', 2)
    checkFunction(fn, 'exports', 2)
    published[name] = fn
  end,
  __index = function(_, resource)
    checkName(resource, 'exports[...]', 2)
    return exportProxy(resource)
  end,
  __newindex = function()
    error("use exports('name', fn) to publish an export", 2)
  end,
})

function internal.callExport(name, ...)
  local fn = published[name]
  if not fn then
    error(("resource '%s' has no export named '%s'"):format(bridge.resourceName, tostring(name)), 0)
  end
  return invoke(fn, 'export:' .. name, ...)
end

function internal.hasExport(name)
  return published[name] ~= nil
end

-------------------------------------------------------------------------------
-- Environment helpers
-------------------------------------------------------------------------------

IS_SERVER = bridge.isServer
IS_CLIENT = not bridge.isServer

function GetCurrentResourceName()
  return bridge.resourceName
end

-- Familiar name: true when running on the server.
function IsDuplicityVersion()
  return IS_SERVER
end

json = { encode = bridge.jsonEncode, decode = bridge.jsonDecode }

-------------------------------------------------------------------------------
-- vector3
-------------------------------------------------------------------------------

local vectorMeta = {}

local function isVector(value)
  return type(value) == 'table' and getmetatable(value) == vectorMeta
end

local function component(value, label)
  if value == nil then return 0.0 end
  local n = tonumber(value)
  if not n then error('vector3: ' .. label .. ' must be a number, got ' .. type(value), 3) end
  return n + 0.0
end

function vector3(x, y, z)
  return setmetatable({ x = component(x, 'x'), y = component(y, 'y'), z = component(z, 'z') }, vectorMeta)
end

local function checkVectors(a, b, op)
  if not (isVector(a) and isVector(b)) then
    error('vector3: attempt to ' .. op .. ' a vector3 and a non-vector3 value', 3)
  end
end

vectorMeta.__add = function(a, b)
  checkVectors(a, b, 'add')
  return vector3(a.x + b.x, a.y + b.y, a.z + b.z)
end

vectorMeta.__sub = function(a, b)
  checkVectors(a, b, 'subtract')
  return vector3(a.x - b.x, a.y - b.y, a.z - b.z)
end

vectorMeta.__mul = function(a, b)
  if type(a) == 'number' then a, b = b, a end
  if not (isVector(a) and type(b) == 'number') then
    error('vector3: multiplication is only defined between a vector3 and a number', 2)
  end
  return vector3(a.x * b, a.y * b, a.z * b)
end

vectorMeta.__div = function(a, b)
  if not (isVector(a) and type(b) == 'number') then
    error('vector3: division is only defined as vector3 / number', 2)
  end
  return vector3(a.x / b, a.y / b, a.z / b)
end

vectorMeta.__unm = function(a)
  return vector3(-a.x, -a.y, -a.z)
end

vectorMeta.__eq = function(a, b)
  return isVector(a) and isVector(b) and a.x == b.x and a.y == b.y and a.z == b.z
end

-- #v is the magnitude.
vectorMeta.__len = function(a)
  return sqrt(a.x * a.x + a.y * a.y + a.z * a.z)
end

vectorMeta.__tostring = function(a)
  return format('vector3(%s, %s, %s)', a.x, a.y, a.z)
end

return internal
