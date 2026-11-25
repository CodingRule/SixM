local MAX_LENGTH = GetConvarInt('chat_max_length', 200)

local function broadcast(author, text)
  print(('%s: %s'):format(author, text))
  TriggerClientEvent('chat:message', -1, author, text)
end

-- A player sent a line of chat.
RegisterNetEvent('chat:send', function(text)
  local player = source
  if type(text) ~= 'string' then return end
  text = text:gsub('%c', ''):sub(1, MAX_LENGTH)
  if text:match('^%s*$') then return end
  broadcast(GetPlayerName(player) or ('player ' .. player), text)
end)

-- `say <text>` from the server console. Restricted: players cannot run it.
RegisterCommand('say', function(_, args)
  if #args == 0 then return print('usage: say <text>') end
  broadcast('console', table.concat(args, ' '))
end, true)

-- Other resources can post to chat:  exports.chat:announce('Server restarting')
exports('announce', function(text)
  broadcast('server', tostring(text))
end)
