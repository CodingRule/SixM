-- Refuse a connection before the player gets a slot.
AddEventHandler('playerConnecting', function(name, setKickReason)
  if #name < 2 then
    setKickReason('Your name must be at least 2 characters long.')
  end
end)

AddEventHandler('playerJoined', function(id, name)
  exports.chat:announce(('%s joined (%d online)'):format(name, GetNumPlayers()))
  TriggerClientEvent('chat:message', id, 'server', GetConvar('welcome_message', 'Welcome to SixM!'))
end)

AddEventHandler('playerDropped', function(reason)
  local name = GetPlayerName(source) or ('player ' .. source)
  exports.chat:announce(('%s left (%s)'):format(name, reason))
end)

-- `players` lists who is online; anyone may run it.
RegisterCommand('players', function(src)
  local lines = {}
  for _, id in ipairs(GetPlayers()) do
    local pos = GetPlayerCoords(id)
    lines[#lines + 1] = ('#%d %s  %dms  (%.1f, %.1f, %.1f)'):format(
      id, GetPlayerName(id), GetPlayerPing(id), pos.x, pos.y, pos.z)
  end
  local text = #lines > 0 and table.concat(lines, '\n') or 'nobody is online'
  if src == 0 then
    print(text)
  else
    TriggerClientEvent('chat:message', src, 'server', text)
  end
end, false)
