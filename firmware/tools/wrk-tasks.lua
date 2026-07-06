-- wrk script: load /api/tasks and analyze the trace counters it returns.
--
-- Usage (single wrk thread gives the cleanest report; -c2 matches POOL_MAX so
-- every keep-alive connection gets a worker, more stresses accept):
--
--   wrk -t1 -c2 -d10s --latency -s firmware/tools/wrk-tasks.lua http://10.42.0.61/api/tasks
--
-- What it checks, per response:
--   * well-formedness (complete JSON with all expected fields — a truncated body
--     would indicate the response no longer fits the worker's tx buffer);
--   * heap_free min/max (headroom under full-pool load — the failure mode that
--     motivated the tx/body sizing);
--   * wrap-safe monotonicity of now_ticks and each task's exec_ticks (a backward
--     step beyond wrap tolerance would mean a trace attribution bug).
-- And over the whole run (first vs last sample, wrap-safe diffs):
--   * per-task CPU%, poll count, average and maximum poll duration (us);
--   * per-executor decomposition: busy% = in-poll% + overhead% (executor
--     bookkeeping + trace hooks + ISRs landing between polls), poll/pass rates,
--     and the unsupervised-task share (in-poll minus the supervised sum).
--
-- All device counters are wrapping u32 ticks (tick_hz gives the unit), so every
-- delta below goes through sub32. The counters wrap after ~71 min of device
-- uptime at 1 MHz — fine for any wrk run; only a run *straddling* two device
-- reboots would confuse the first/last comparison.
--
-- Implementation note: each wrk thread runs an isolated Lua state, and done()
-- runs in yet another. thread:get() is only reliable for SCALAR values — copying
-- a nested table across states segfaults some wrk builds — so each thread keeps
-- its stats local and re-renders the full report into a plain STRING after every
-- response; done() fetches and prints that string.

local WRAP = 2 ^ 32
local function sub32(a, b)
    return (a - b) % WRAP
end

-- Collect thread handles so done() can pull each thread's rendered report out.
local threads = {}

function setup(thread)
    table.insert(threads, thread)
end

function init(args)
    stats = {
        samples = 0,
        bad_status = 0,
        malformed = 0,
        regressions = 0,
        heap_min = math.huge,
        heap_max = 0,
        body_min = math.huge,
        body_max = 0,
        tick_hz = nil,
        first_now = nil,
        last_now = nil,
        prev_now = nil,
        tasks = {}, -- name -> {first_e, last_e, prev_e, first_p, last_p, maxp}
        execs = {}, -- id   -> {first_i, last_i}
    }
    report = "" -- the string done() fetches; re-rendered after every response
end

local function render()
    local s = stats
    local out = {}
    local function line(...)
        table.insert(out, string.format(...))
    end

    line("samples %d | non-200 %d | malformed/truncated %d | counter regressions %d",
        s.samples, s.bad_status, s.malformed, s.regressions)
    if s.body_max > 0 then
        line("body bytes  min %d / max %d (worker tx buffer must hold max + headers)",
            s.body_min, s.body_max)
        line("heap_free   min %d / max %d B (min = headroom at peak load)",
            s.heap_min, s.heap_max)
    end

    local hz = s.tick_hz
    local dnow = s.first_now and sub32(s.last_now, s.first_now) or 0
    if hz and dnow > 0 then
        line("window      %.1f s of device time (tick_hz %d)", dnow / hz, hz)

        -- Sum of supervised task time, for the unsupervised share below (only
        -- attributable to an executor when there is exactly one).
        local nodes_de, nexecs = 0, 0
        for _, t in pairs(s.tasks) do
            nodes_de = nodes_de + sub32(t.last_e, t.first_e)
        end
        for _ in pairs(s.execs) do
            nexecs = nexecs + 1
        end

        for id, x in pairs(s.execs) do
            local busy = 100 * (1 - math.min(1, sub32(x.last_i, x.first_i) / dnow))
            local inpoll = 100 * sub32(x.last_e, x.first_e) / dnow
            local polls = sub32(x.last_p, x.first_p)
            local passes = sub32(x.last_pa, x.first_pa)
            line("executor %08x: %.1f%% busy = %.1f%% in-poll + %.1f%% overhead"
                .. " (scheduler + hooks + inter-poll ISRs)",
                id, busy, inpoll, math.max(0, busy - inpoll))
            line("executor %08x: %.0f polls/s, %.0f passes/s, %.2f polls/pass%s",
                id, polls / (dnow / hz), passes / (dnow / hz),
                passes > 0 and polls / passes or 0,
                nexecs == 1 and string.format(
                    ", unsupervised tasks %.2f%%",
                    math.max(0, 100 * (sub32(x.last_e, x.first_e) - nodes_de) / dnow)) or "")
        end

        line("%-12s %7s %10s %12s %12s", "task", "cpu%", "polls", "avg poll us", "max poll us")
        local names = {}
        for name in pairs(s.tasks) do
            table.insert(names, name)
        end
        table.sort(names)
        for _, name in ipairs(names) do
            local t = s.tasks[name]
            local de = sub32(t.last_e, t.first_e)
            local dp = sub32(t.last_p, t.first_p)
            line("%-12s %6.2f%% %10d %12.1f %12.1f",
                name,
                100 * de / dnow,
                dp,
                dp > 0 and (de / dp) * 1e6 / hz or 0,
                t.maxp * 1e6 / hz)
        end
    end
    return table.concat(out, "\n")
end

function response(status, headers, body)
    stats.samples = stats.samples + 1
    if status ~= 200 then
        stats.bad_status = stats.bad_status + 1
        return
    end

    stats.body_min = math.min(stats.body_min, #body)
    stats.body_max = math.max(stats.body_max, #body)

    local heap = tonumber(body:match('"heap_free":(%d+)'))
    local hz = tonumber(body:match('"tick_hz":(%d+)'))
    local now = tonumber(body:match('"now_ticks":(%d+)'))
    -- A complete body ends the tasks array and the object; anything else means
    -- the device truncated the response.
    if not (heap and hz and now and body:sub(-2) == "]}") then
        stats.malformed = stats.malformed + 1
        report = render()
        return
    end

    stats.tick_hz = hz
    stats.heap_min = math.min(stats.heap_min, heap)
    stats.heap_max = math.max(stats.heap_max, heap)
    stats.first_now = stats.first_now or now
    -- now_ticks must move forward (wrap-safe): a delta in the top half of the
    -- u32 range reads as a backward step.
    if stats.prev_now and sub32(now, stats.prev_now) > 2 ^ 31 then
        stats.regressions = stats.regressions + 1
    end
    stats.prev_now = now
    stats.last_now = now

    for name, e, p, mp in body:gmatch(
        '"name":"([^"]+)".-"exec_ticks":(%d+),"polls":(%d+),"max_poll_ticks":(%d+)'
    ) do
        e, p, mp = tonumber(e), tonumber(p), tonumber(mp)
        local t = stats.tasks[name]
        if not t then
            t = { first_e = e, first_p = p, maxp = 0 }
            stats.tasks[name] = t
        end
        -- exec_ticks only ever accumulates; backward (beyond wrap) = attribution bug.
        if t.prev_e and sub32(e, t.prev_e) > 2 ^ 31 then
            stats.regressions = stats.regressions + 1
        end
        t.prev_e, t.last_e, t.last_p = e, e, p
        t.maxp = math.max(t.maxp, mp)
    end

    for id, idle, exec, polls, passes in body:gmatch(
        '{"id":(%d+),"idle_ticks":(%d+),"exec_ticks":(%d+),"polls":(%d+),"passes":(%d+)}'
    ) do
        id = tonumber(id)
        local x = stats.execs[id]
        if not x then
            x = { first_i = tonumber(idle), first_e = tonumber(exec), first_p = tonumber(polls) }
            stats.execs[id] = x
        end
        x.last_i, x.last_e = tonumber(idle), tonumber(exec)
        x.last_p, x.last_pa = tonumber(polls), tonumber(passes)
        x.first_pa = x.first_pa or tonumber(passes)
    end

    report = render()
end

function done(summary, latency, requests)
    for ti, thread in ipairs(threads) do
        local r = thread:get("report")
        if r and #r > 0 then
            io.write(string.format("\n==== /api/tasks analysis (wrk thread %d) ====\n", ti))
            io.write(r, "\n")
        end
    end
end
