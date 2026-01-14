#!/usr/bin/env python3
"""
ime_server.py - 本地输入法透传服务
接收浏览器发送的文本，使用 xdotool 输入到当前焦点窗口

工作原理:
1. 用户在浏览器中使用本地输入法（如搜狗输入法）输入中文
2. 前端通过 WebSocket 发送完整的文本到此服务
3. 此服务使用 xdotool 将文本输入到远程桌面的当前焦点窗口

端口: 6091 (可通过 IME_PORT 环境变量配置)
"""

import asyncio
import json
import subprocess
import os
import signal
import sys
from datetime import datetime

# 尝试导入 websockets，如果失败则提供友好提示
try:
    from websockets.server import serve
    from websockets.exceptions import ConnectionClosed
except ImportError:
    print("[IME] Error: websockets module not found. Install with: pip3 install websockets")
    sys.exit(1)


def log(level: str, message: str):
    """格式化日志输出"""
    timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{timestamp}] [{level}] [IME] {message}", flush=True)


def sanitize_text(text: str) -> str:
    """
    清理文本，防止命令注入和异常字符

    安全检查:
    1. 长度限制 (1000 字符)
    2. 过滤危险控制字符 (NULL, ESC)
    3. 使用 -- 参数分隔符防止注入

    Args:
        text: 待验证的文本

    Returns:
        清理后的文本

    Raises:
        ValueError: 如果文本包含危险内容
    """
    # 1. 长度限制
    if len(text) > 1000:
        raise ValueError("Text too long (max 1000 chars)")

    # 2. 过滤危险控制字符
    dangerous_chars = ['\x00', '\x1b']  # NULL 和 ESC
    if any(c in text for c in dangerous_chars):
        raise ValueError("Text contains dangerous control characters")

    return text


def type_text_xdotool(text: str) -> tuple[bool, str]:
    """
    使用 xdotool 输入文本
    
    Args:
        text: 要输入的文本
        
    Returns:
        (success, error_message)
    """
    try:
        env = {**os.environ, 'DISPLAY': ':0'}
        
        # 使用 xdotool type 输入文本
        # --clearmodifiers: 清除修饰键状态（避免 Ctrl/Alt 等键干扰）
        # --delay: 每个字符之间的延迟（毫秒），帮助应用程序处理
        result = subprocess.run(
            ['xdotool', 'type', '--clearmodifiers', '--delay', '10', '--', text],
            env=env,
            capture_output=True,
            text=True,
            timeout=10
        )
        
        if result.returncode == 0:
            return True, ""
        else:
            return False, result.stderr.strip()
            
    except subprocess.TimeoutExpired:
        return False, "xdotool timeout"
    except FileNotFoundError:
        return False, "xdotool not found"
    except Exception as e:
        return False, str(e)


def type_text_clipboard(text: str) -> tuple[bool, str]:
    """
    备用方案: 通过剪贴板粘贴文本
    当 xdotool type 对某些字符不支持时使用
    
    Args:
        text: 要输入的文本
        
    Returns:
        (success, error_message)
    """
    try:
        env = {**os.environ, 'DISPLAY': ':0'}
        
        # 1. 将文本写入剪贴板
        process = subprocess.Popen(
            ['xclip', '-selection', 'clipboard'],
            stdin=subprocess.PIPE,
            env=env
        )
        process.communicate(input=text.encode('utf-8'), timeout=5)
        
        if process.returncode != 0:
            return False, "Failed to copy to clipboard"
        
        # 2. 模拟 Ctrl+V 粘贴
        result = subprocess.run(
            ['xdotool', 'key', '--clearmodifiers', 'ctrl+v'],
            env=env,
            capture_output=True,
            text=True,
            timeout=5
        )
        
        if result.returncode == 0:
            return True, ""
        else:
            return False, result.stderr.strip()
            
    except subprocess.TimeoutExpired:
        return False, "Clipboard operation timeout"
    except FileNotFoundError as e:
        return False, f"Required tool not found: {e}"
    except Exception as e:
        return False, str(e)


async def handle_client(websocket):
    """处理来自前端的 WebSocket 连接"""
    client_addr = websocket.remote_address
    log("INFO", f"Client connected: {client_addr}")
    
    try:
        async for message in websocket:
            try:
                data = json.loads(message)
                msg_type = data.get('type', '')
                
                if msg_type == 'text':
                    text = data.get('text', '')
                    method = data.get('method', 'xdotool')  # 默认使用 xdotool

                    if not text:
                        await websocket.send(json.dumps({
                            'status': 'error',
                            'message': 'Empty text'
                        }))
                        continue

                    # 安全验证
                    try:
                        text = sanitize_text(text)
                    except ValueError as e:
                        log("WARN", f"Text validation failed: {e}")
                        await websocket.send(json.dumps({
                            'status': 'error',
                            'message': str(e)
                        }))
                        continue

                    # 根据方法选择输入方式
                    if method == 'clipboard':
                        success, error = type_text_clipboard(text)
                    else:
                        success, error = type_text_xdotool(text)
                    
                    if success:
                        log("INFO", f"Typed ({method}): {text[:50]}{'...' if len(text) > 50 else ''}")
                        await websocket.send(json.dumps({'status': 'ok'}))
                    else:
                        log("ERROR", f"Failed to type: {error}")
                        await websocket.send(json.dumps({
                            'status': 'error',
                            'message': error
                        }))
                        
                elif msg_type == 'ping':
                    # 心跳检测
                    await websocket.send(json.dumps({'type': 'pong'}))
                    
                else:
                    log("WARN", f"Unknown message type: {msg_type}")
                    await websocket.send(json.dumps({
                        'status': 'error',
                        'message': f'Unknown type: {msg_type}'
                    }))
                    
            except json.JSONDecodeError:
                log("ERROR", f"Invalid JSON: {message[:100]}")
                await websocket.send(json.dumps({
                    'status': 'error',
                    'message': 'Invalid JSON'
                }))
                
    except ConnectionClosed:
        log("INFO", f"Client disconnected: {client_addr}")
    except Exception as e:
        log("ERROR", f"Client error: {e}")
    finally:
        log("INFO", f"Connection closed: {client_addr}")


async def main():
    """主函数"""
    port = int(os.environ.get('IME_PORT', 6091))
    host = os.environ.get('IME_HOST', '0.0.0.0')
    
    log("INFO", f"Starting IME input server on {host}:{port}...")
    log("INFO", "Protocol: WebSocket")
    log("INFO", "Message format: {\"type\": \"text\", \"text\": \"要输入的文本\"}")
    
    # 优雅关闭处理
    stop = asyncio.Event()
    
    def signal_handler():
        log("INFO", "Received shutdown signal")
        stop.set()
    
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, signal_handler)
    
    async with serve(handle_client, host, port):
        log("INFO", f"IME server is ready, listening on ws://{host}:{port}")
        await stop.wait()
    
    log("INFO", "IME server stopped")


if __name__ == '__main__':
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        log("INFO", "Server interrupted")
    except Exception as e:
        log("ERROR", f"Server error: {e}")
        sys.exit(1)
