RegisterNetEvent('chat:message', function(author, text)
  print(('<%s> %s'):format(author, text))
end)

-- `say hello there` (or `/say hello there`) sends a line of chat.
RegisterCommand('say', function(_, args)
  if #args > 0 then
    TriggerServerEvent('chat:send', table.concat(args, ' '))
  end
end, false)
