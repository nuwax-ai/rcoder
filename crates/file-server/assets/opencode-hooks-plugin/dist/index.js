import { loadConfig, warnUnsupportedEvents } from "./config.js";
import { matchHook } from "./matcher.js";
import { executeHook } from "./executor.js";
import { buildToolHookInput, buildSessionHookInput, mapToolAfterEvent, } from "./events.js";
const LOG_PREFIX = "[opencode-hooks-api]";
async function runHooksForEvent(eventName, matchers, toolName, input, client) {
    if (!matchers || matchers.length === 0)
        return [];
    const promises = [];
    for (const matcher of matchers) {
        if (!matchHook(matcher.matcher, toolName))
            continue;
        for (const handler of matcher.hooks) {
            promises.push(executeHook(handler, input, client).catch((err) => {
                const message = err instanceof Error ? err.message : String(err);
                console.warn(`${LOG_PREFIX} Hook error (${eventName}):`, message);
                return { action: "error", reason: message };
            }));
        }
    }
    return Promise.all(promises);
}
function findBlockResult(results) {
    return results.find((r) => r.action === "block");
}
function findUpdatedInput(results) {
    for (const r of results) {
        if (r.updatedInput)
            return r.updatedInput;
    }
    return undefined;
}
function logAdditionalContext(results, eventName) {
    for (const r of results) {
        if (r.additionalContext) {
            console.log(`${LOG_PREFIX} [${eventName}] additionalContext: ${r.additionalContext}`);
        }
    }
}
function extractSessionId(properties) {
    // session.idle / session.compacted → properties.sessionID
    if (typeof properties.sessionID === "string")
        return properties.sessionID;
    // session.created / session.updated / session.deleted → properties.info.id
    if (properties.info &&
        typeof properties.info === "object" &&
        "id" in properties.info &&
        typeof properties.info.id === "string") {
        return properties.info.id;
    }
    return "";
}
async function runSlashCommandHooks(eventName, matchers, sessionId, directory, client) {
    if (!matchers || matchers.length === 0)
        return;
    const input = {
        session_id: sessionId,
        cwd: directory,
        hook_event_name: eventName,
    };
    process.env.OPENCODE_SESSION_ID = sessionId;
    process.env.OPENCODE_HOOK_EVENT = eventName;
    const results = await runHooksForEvent(eventName, matchers, undefined, input, client);
    const block = findBlockResult(results);
    if (block) {
        console.log(`${LOG_PREFIX} [${eventName}] Hook requested block: ${block.reason ?? "no reason"}`);
    }
    logAdditionalContext(results, eventName);
}
// ---------------------------------------------------------------------------
// Plugin entry point
// ---------------------------------------------------------------------------
export const HooksPlugin = async ({ directory, client }) => {
    const { hooks, disableAllHooks } = loadConfig(directory);
    if (disableAllHooks) {
        console.log(`${LOG_PREFIX} All hooks disabled (disableAllHooks=true)`);
        return {};
    }
    warnUnsupportedEvents(hooks);
    process.env.OPENCODE_PROJECT_DIR = directory;
    const toolBefore = async (input, output) => {
        const eventName = "PreToolUse";
        const matchers = hooks[eventName];
        if (!matchers || matchers.length === 0)
            return;
        const ocEvent = {
            sessionId: input.sessionID,
            tool: input.tool,
            args: output.args ?? {},
        };
        const hookInput = buildToolHookInput(eventName, ocEvent, directory);
        process.env.OPENCODE_SESSION_ID = input.sessionID;
        process.env.OPENCODE_HOOK_EVENT = eventName;
        const results = await runHooksForEvent(eventName, matchers, input.tool, hookInput, client);
        const block = findBlockResult(results);
        if (block) {
            throw new Error(block.reason ?? "Blocked by PreToolUse hook");
        }
        const updatedInput = findUpdatedInput(results);
        if (updatedInput) {
            Object.assign(output.args, updatedInput);
        }
        logAdditionalContext(results, eventName);
    };
    const toolAfter = async (input, output) => {
        const ocEvent = {
            sessionId: input.sessionID,
            tool: input.tool,
            args: input.args ?? {},
            output: output.output,
        };
        const eventName = mapToolAfterEvent(ocEvent);
        const matchers = hooks[eventName];
        if (!matchers || matchers.length === 0)
            return;
        const hookInput = buildToolHookInput(eventName, ocEvent, directory);
        process.env.OPENCODE_SESSION_ID = input.sessionID;
        process.env.OPENCODE_HOOK_EVENT = eventName;
        const results = await runHooksForEvent(eventName, matchers, input.tool, hookInput, client);
        const block = findBlockResult(results);
        if (block) {
            console.log(`${LOG_PREFIX} [${eventName}] Hook requested block: ${block.reason ?? "no reason"}`);
        }
        logAdditionalContext(results, eventName);
    };
    const onEvent = async ({ event, }) => {
        try {
            switch (event.type) {
                // ---- Session lifecycle ----
                case "session.created": {
                    const matchers = hooks["SessionStart"];
                    if (!matchers || matchers.length === 0)
                        return;
                    const sessionId = extractSessionId(event.properties);
                    const ocEvent = { sessionId };
                    const hookInput = buildSessionHookInput("SessionStart", ocEvent, directory);
                    process.env.OPENCODE_SESSION_ID = sessionId;
                    process.env.OPENCODE_HOOK_EVENT = "SessionStart";
                    const results = await runHooksForEvent("SessionStart", matchers, undefined, hookInput, client);
                    const block = findBlockResult(results);
                    if (block) {
                        console.log(`${LOG_PREFIX} [SessionStart] Hook requested block: ${block.reason ?? "no reason"}`);
                    }
                    logAdditionalContext(results, "SessionStart");
                    break;
                }
                case "session.deleted": {
                    const matchers = hooks["SessionEnd"];
                    if (!matchers || matchers.length === 0)
                        return;
                    const sessionId = extractSessionId(event.properties);
                    const ocEvent = { sessionId };
                    const hookInput = buildSessionHookInput("SessionEnd", ocEvent, directory);
                    process.env.OPENCODE_SESSION_ID = sessionId;
                    process.env.OPENCODE_HOOK_EVENT = "SessionEnd";
                    const results = await runHooksForEvent("SessionEnd", matchers, undefined, hookInput, client);
                    const block = findBlockResult(results);
                    if (block) {
                        console.log(`${LOG_PREFIX} [SessionEnd] Hook requested block: ${block.reason ?? "no reason"}`);
                    }
                    logAdditionalContext(results, "SessionEnd");
                    break;
                }
                case "session.idle": {
                    const matchers = hooks["Stop"];
                    if (!matchers || matchers.length === 0)
                        return;
                    const sessionId = extractSessionId(event.properties);
                    const ocEvent = { sessionId };
                    const hookInput = buildSessionHookInput("Stop", ocEvent, directory);
                    process.env.OPENCODE_SESSION_ID = sessionId;
                    process.env.OPENCODE_HOOK_EVENT = "Stop";
                    const results = await runHooksForEvent("Stop", matchers, undefined, hookInput, client);
                    const block = findBlockResult(results);
                    if (block) {
                        console.log(`${LOG_PREFIX} [Stop] Hook requested block: ${block.reason ?? "no reason"}`);
                    }
                    logAdditionalContext(results, "Stop");
                    break;
                }
                // ---- Slash command stubs ----
                case "command.executed": {
                    const props = event.properties;
                    const commandName = props.name ?? "";
                    const sessionId = props.sessionID ?? "";
                    if (commandName === "hook-prompt") {
                        await runSlashCommandHooks("UserPromptSubmit", hooks["UserPromptSubmit"], sessionId, directory, client);
                    }
                    else if (commandName === "hook-notify") {
                        await runSlashCommandHooks("Notification", hooks["Notification"], sessionId, directory, client);
                    }
                    else if (commandName === "hook-precompact") {
                        await runSlashCommandHooks("PreCompact", hooks["PreCompact"], sessionId, directory, client);
                    }
                    break;
                }
                default:
                    // Ignore all other events
                    break;
            }
        }
        catch (err) {
            const message = err instanceof Error ? err.message : String(err);
            console.warn(`${LOG_PREFIX} Unhandled error in event handler (${event.type}):`, message);
        }
    };
    return {
        "tool.execute.before": toolBefore,
        "tool.execute.after": toolAfter,
        event: onEvent,
    };
};
export default HooksPlugin;
//# sourceMappingURL=index.js.map