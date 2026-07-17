import { spawn } from "node:child_process";
// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function parseHookOutput(text) {
    const trimmed = text.trim();
    if (!trimmed)
        return null;
    try {
        return JSON.parse(trimmed);
    }
    catch {
        return null;
    }
}
function outputToResult(output) {
    if (!output)
        return { action: "allow" };
    // Block conditions: explicit decision, continue=false, or ok=false
    if (output.decision === "block" || output.continue === false || output.ok === false) {
        return {
            action: "block",
            reason: output.stopReason ?? output.reason ?? "Blocked by hook",
            updatedInput: output.hookSpecificOutput?.updatedInput,
            additionalContext: output.hookSpecificOutput?.additionalContext,
        };
    }
    return {
        action: "allow",
        updatedInput: output.hookSpecificOutput?.updatedInput,
        additionalContext: output.hookSpecificOutput?.additionalContext,
    };
}
function interpolateEnvVars(value) {
    return value.replace(/\$([A-Z_][A-Z0-9_]*)/g, (_, name) => process.env[name] ?? "");
}
// ---------------------------------------------------------------------------
// Command handler
// ---------------------------------------------------------------------------
async function runCommand(handler, input) {
    const timeoutMs = (handler.timeout ?? 600) * 1000;
    const inputJson = JSON.stringify(input);
    return new Promise((resolve) => {
        let settled = false;
        const proc = spawn("bash", ["-c", handler.command], {
            env: {
                ...process.env,
                OPENCODE_PROJECT_DIR: input.cwd,
                OPENCODE_SESSION_ID: input.session_id,
                OPENCODE_HOOK_EVENT: input.hook_event_name,
            },
        });
        const stdoutChunks = [];
        const stderrChunks = [];
        proc.stdout.on("data", (chunk) => stdoutChunks.push(chunk));
        proc.stderr.on("data", (chunk) => stderrChunks.push(chunk));
        const timer = setTimeout(() => {
            if (!settled) {
                settled = true;
                proc.kill("SIGKILL");
                resolve({ action: "error", reason: `Command timed out after ${handler.timeout ?? 600}s` });
            }
        }, timeoutMs);
        proc.on("close", (code) => {
            if (settled)
                return;
            settled = true;
            clearTimeout(timer);
            const stdout = Buffer.concat(stdoutChunks).toString("utf-8");
            const stderr = Buffer.concat(stderrChunks).toString("utf-8").trim();
            if (code === 0) {
                const output = parseHookOutput(stdout);
                resolve(outputToResult(output));
            }
            else if (code === 2) {
                resolve({ action: "block", reason: stderr || "Blocked by hook" });
            }
            else {
                // Non-zero, non-2: non-blocking error
                const reason = stderr || `Command exited with code ${code}`;
                console.error(`[opencode-hooks-api] Command hook error (exit ${code}): ${reason}`);
                resolve({ action: "error", reason });
            }
        });
        proc.on("error", (err) => {
            if (settled)
                return;
            settled = true;
            clearTimeout(timer);
            console.error(`[opencode-hooks-api] Failed to spawn command: ${err.message}`);
            resolve({ action: "error", reason: err.message });
        });
        // Write input JSON to stdin then close it
        proc.stdin.write(inputJson, () => {
            proc.stdin.end();
        });
    });
}
// ---------------------------------------------------------------------------
// HTTP handler
// ---------------------------------------------------------------------------
async function runHttp(handler, input) {
    const timeoutMs = (handler.timeout ?? 600) * 1000;
    const body = JSON.stringify(input);
    const headers = { "Content-Type": "application/json" };
    if (handler.headers) {
        for (const [key, value] of Object.entries(handler.headers)) {
            headers[key] = interpolateEnvVars(value);
        }
    }
    try {
        const response = await fetch(handler.url, {
            method: "POST",
            headers,
            body,
            signal: AbortSignal.timeout(timeoutMs),
        });
        if (!response.ok) {
            const reason = `HTTP ${response.status} ${response.statusText}`;
            console.error(`[opencode-hooks-api] HTTP hook error: ${reason}`);
            return { action: "error", reason };
        }
        const text = await response.text();
        const output = parseHookOutput(text);
        return outputToResult(output);
    }
    catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        console.error(`[opencode-hooks-api] HTTP hook fetch error: ${message}`);
        return { action: "error", reason: message };
    }
}
// ---------------------------------------------------------------------------
// Prompt / Agent handler
// ---------------------------------------------------------------------------
async function runPrompt(handler, input, client) {
    if (!client || typeof client.session !== "object") {
        return { action: "error", reason: "OpenCode SDK client not available for prompt/agent hook" };
    }
    const sdkClient = client;
    const timeoutMs = (handler.timeout ?? (handler.type === "agent" ? 60 : 30)) * 1000;
    const promptText = handler.prompt.replace(/\$ARGUMENTS/g, JSON.stringify(input));
    try {
        const result = await sdkClient.session.prompt({ prompt: promptText, timeout: timeoutMs });
        const output = parseHookOutput(result.text);
        return outputToResult(output);
    }
    catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        console.error(`[opencode-hooks-api] Prompt/agent hook error: ${message}`);
        return { action: "error", reason: message };
    }
}
// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------
export async function executeHook(handler, input, client) {
    switch (handler.type) {
        case "command":
            return runCommand(handler, input);
        case "http":
            return runHttp(handler, input);
        case "prompt":
            return runPrompt(handler, input, client);
        case "agent":
            return runPrompt(handler, input, client);
        default: {
            // Exhaustiveness guard — TypeScript should never reach here
            const _exhaustive = handler;
            void _exhaustive;
            return { action: "skip", reason: "Unknown handler type" };
        }
    }
}
// Re-export helpers for testing
export { parseHookOutput, outputToResult, interpolateEnvVars };
//# sourceMappingURL=executor.js.map