RegisterNetEvent('hello:pong', function(sentAt, online)
  print(('pong: round trip %d ms, %d player(s) online'):format(GetGameTimer() - sentAt, online))
end)

-- `hello` asks the server to answer.
RegisterCommand('hello', function()
  TriggerServerEvent('hello:ping', GetGameTimer())
end, false)

-- `whereami` prints your position; `near` lists the players streamed to you.
RegisterCommand('whereami', function()
  local pos = GetLocalCoords()
  print(('you are at %.1f, %.1f, %.1f'):format(pos.x, pos.y, pos.z))
end, false)

RegisterCommand('near', function()
  local me = GetLocalCoords()
  for _, id in ipairs(GetActivePlayers()) do
    if id ~= GetLocalPlayerId() and IsPlayerStreamed(id) then
      local p = GetPlayerCoords(id)
      local d = #(vector3(p.x, p.y, p.z) - vector3(me.x, me.y, me.z))
      print(('#%d %s is %.1f m away'):format(id, GetPlayerName(id), d))
    end
  end
end, false)
