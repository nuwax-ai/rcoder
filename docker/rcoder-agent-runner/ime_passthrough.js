/**
 * ime_passthrough.js - 本地输入法透传前端脚本
 * 
 * 使用方法：
 * 1. 将此文件放到 noVNC 目录中
 * 2. 在 vnc.html 的 </body> 前添加：
 *    <script src="ime_passthrough.js"></script>
 * 
 * 工作原理：
 * 1. 创建一个隐藏的输入框，用于捕获用户的输入法输入
 * 2. 当用户点击 VNC 画面时，焦点切换到隐藏输入框
 * 3. 用户使用本地输入法（如搜狗输入法）输入中文
 * 4. 输入完成后，通过 WebSocket 发送到 IME 服务器
 * 5. 服务器使用 xdotool 将文本输入到远程桌面
 */

(function () {
    'use strict';

    // 配置
    const CONFIG = {
        // IME 服务端口（与 ime_server.py 中的端口一致）
        IME_PORT: 6091,
        // 重连间隔（毫秒）
        RECONNECT_INTERVAL: 3000,
        // 心跳间隔（毫秒）
        HEARTBEAT_INTERVAL: 30000,
        // 是否启用调试日志
        DEBUG: false
    };

    // 日志函数
    function log(level, message) {
        if (level === 'DEBUG' && !CONFIG.DEBUG) return;
        const timestamp = new Date().toISOString();
        console.log(`[${timestamp}] [${level}] [IME] ${message}`);
    }

    // 状态变量
    let imeWebSocket = null;
    let imeInput = null;
    let isComposing = false;
    let reconnectTimer = null;
    let heartbeatTimer = null;

    /**
     * 创建隐藏的输入框
     */
    function createImeInput() {
        if (imeInput) return imeInput;

        imeInput = document.createElement('textarea');
        imeInput.id = 'ime-capture';
        imeInput.setAttribute('autocomplete', 'off');
        imeInput.setAttribute('autocorrect', 'off');
        imeInput.setAttribute('autocapitalize', 'off');
        imeInput.setAttribute('spellcheck', 'false');
        imeInput.style.cssText = `
            position: fixed;
            left: -9999px;
            top: 50%;
            width: 1px;
            height: 1px;
            opacity: 0;
            pointer-events: none;
            z-index: -1;
        `;

        document.body.appendChild(imeInput);

        // 监听输入法事件
        imeInput.addEventListener('compositionstart', onCompositionStart);
        imeInput.addEventListener('compositionend', onCompositionEnd);
        imeInput.addEventListener('input', onInput);
        imeInput.addEventListener('keydown', onKeyDown);

        log('INFO', 'IME input element created');
        return imeInput;
    }

    /**
     * 连接到 IME WebSocket 服务
     */
    function connectWebSocket() {
        if (imeWebSocket && imeWebSocket.readyState === WebSocket.OPEN) {
            return;
        }

        // 构建 WebSocket URL（使用当前页面的主机名）
        const wsUrl = `ws://${location.hostname}:${CONFIG.IME_PORT}`;
        log('INFO', `Connecting to IME server: ${wsUrl}`);

        try {
            imeWebSocket = new WebSocket(wsUrl);

            imeWebSocket.onopen = function () {
                log('INFO', 'Connected to IME server');
                clearReconnectTimer();
                startHeartbeat();
            };

            imeWebSocket.onclose = function (event) {
                log('INFO', `Disconnected from IME server (code: ${event.code})`);
                stopHeartbeat();
                scheduleReconnect();
            };

            imeWebSocket.onerror = function (error) {
                log('ERROR', 'WebSocket error');
                // onclose 会自动触发
            };

            imeWebSocket.onmessage = function (event) {
                try {
                    const data = JSON.parse(event.data);
                    if (data.status === 'ok') {
                        log('DEBUG', 'Text sent successfully');
                    } else if (data.status === 'error') {
                        log('ERROR', `Server error: ${data.message}`);
                    } else if (data.type === 'pong') {
                        log('DEBUG', 'Heartbeat pong received');
                    }
                } catch (e) {
                    log('ERROR', `Failed to parse response: ${e}`);
                }
            };
        } catch (e) {
            log('ERROR', `Failed to connect: ${e}`);
            scheduleReconnect();
        }
    }

    /**
     * 发送文本到 IME 服务器
     */
    function sendText(text, method = 'xdotool') {
        if (!text) return;

        if (!imeWebSocket || imeWebSocket.readyState !== WebSocket.OPEN) {
            log('WARN', 'WebSocket not connected, text not sent');
            return;
        }

        const message = JSON.stringify({
            type: 'text',
            text: text,
            method: method
        });

        try {
            imeWebSocket.send(message);
            log('DEBUG', `Sent text: ${text.substring(0, 50)}${text.length > 50 ? '...' : ''}`);
        } catch (e) {
            log('ERROR', `Failed to send text: ${e}`);
        }
    }

    /**
     * 输入法开始组合
     */
    function onCompositionStart(e) {
        isComposing = true;
        log('DEBUG', 'Composition started');
    }

    /**
     * 输入法完成组合
     */
    function onCompositionEnd(e) {
        isComposing = false;
        const text = e.data;

        if (text) {
            log('DEBUG', `Composition ended: ${text}`);
            sendText(text);
        }

        // 清空输入框
        setTimeout(() => {
            if (imeInput) imeInput.value = '';
        }, 10);
    }

    /**
     * 普通输入（非输入法）
     */
    function onInput(e) {
        // 如果正在使用输入法组合，不处理
        if (isComposing) return;

        const text = imeInput.value;
        if (text) {
            log('DEBUG', `Direct input: ${text}`);
            sendText(text);
            imeInput.value = '';
        }
    }

    /**
     * 键盘按下事件
     * 用于处理特殊键（如回车、退格等）
     */
    function onKeyDown(e) {
        // 如果正在使用输入法组合，让输入法处理
        if (isComposing) return;

        // 这些键由 VNC 处理，不需要 IME 透传
        // 用户可以点击 VNC 画面来让 VNC 处理键盘
    }

    /**
     * 定时重连
     */
    function scheduleReconnect() {
        if (reconnectTimer) return;

        log('INFO', `Will reconnect in ${CONFIG.RECONNECT_INTERVAL}ms`);
        reconnectTimer = setTimeout(() => {
            reconnectTimer = null;
            connectWebSocket();
        }, CONFIG.RECONNECT_INTERVAL);
    }

    function clearReconnectTimer() {
        if (reconnectTimer) {
            clearTimeout(reconnectTimer);
            reconnectTimer = null;
        }
    }

    /**
     * 心跳保活
     */
    function startHeartbeat() {
        stopHeartbeat();
        heartbeatTimer = setInterval(() => {
            if (imeWebSocket && imeWebSocket.readyState === WebSocket.OPEN) {
                try {
                    imeWebSocket.send(JSON.stringify({ type: 'ping' }));
                    log('DEBUG', 'Heartbeat ping sent');
                } catch (e) {
                    log('ERROR', `Heartbeat failed: ${e}`);
                }
            }
        }, CONFIG.HEARTBEAT_INTERVAL);
    }

    function stopHeartbeat() {
        if (heartbeatTimer) {
            clearInterval(heartbeatTimer);
            heartbeatTimer = null;
        }
    }

    /**
     * 初始化 IME 透传功能
     */
    function init() {
        log('INFO', 'Initializing IME passthrough...');

        // 创建隐藏输入框
        createImeInput();

        // 连接 WebSocket
        connectWebSocket();

        // 监听 VNC 画面点击，切换焦点到隐藏输入框
        // 需要找到 noVNC 的 screen 元素
        const setupClickHandler = () => {
            // noVNC 可能使用不同的元素 ID
            const vncScreen = document.getElementById('screen') ||
                document.querySelector('canvas') ||
                document.querySelector('.noVNC_canvas');

            if (vncScreen) {
                vncScreen.addEventListener('click', () => {
                    if (imeInput) {
                        imeInput.focus();
                        log('DEBUG', 'IME input focused');
                    }
                });
                log('INFO', 'VNC screen click handler attached');
            } else {
                log('WARN', 'VNC screen element not found, will retry');
                setTimeout(setupClickHandler, 1000);
            }
        };

        // 等待 DOM 完全加载
        if (document.readyState === 'loading') {
            document.addEventListener('DOMContentLoaded', setupClickHandler);
        } else {
            setupClickHandler();
        }

        log('INFO', 'IME passthrough initialized');
        log('INFO', 'Usage: Click on VNC screen, then type using your local input method');
    }

    // 页面加载后初始化
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', init);
    } else {
        init();
    }

    // 导出到全局（方便调试）
    window.IMEPassthrough = {
        sendText: sendText,
        connect: connectWebSocket,
        isConnected: () => imeWebSocket && imeWebSocket.readyState === WebSocket.OPEN,
        setDebug: (enabled) => { CONFIG.DEBUG = enabled; }
    };

})();
