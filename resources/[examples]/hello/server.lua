-- Net events must be registered before clients may trigger them.
RegisterNetEvent('hello:ping', function(sentAt)
  -- `source` is the id of the player who sent the event.
  TriggerClientEvent('hello:pong', source, sentAt, GetNumPlayers())
end)

-- A thread: runs alongside everything else and sleeps with Wait.
CreateThread(function()
  local interval = GetConvarInt('hello_heartbeat_seconds', 60) * 1000
  while true do
    Wait(interval)
    if GetNumPlayers() > 0 then
      print(('%d player(s) online'):format(GetNumPlayers()))
    end
  end
end)
