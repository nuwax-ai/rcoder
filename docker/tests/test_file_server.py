#!/usr/bin/env python3
"""
file-server API 测试套件 (Python3 标准库, 无需 pip install)

用法:
  python3 docker/tests/test_file_server.py                    # 默认 http://127.0.0.1:60001
  python3 docker/tests/test_file_server.py --base http://...  # 指定地址
  python3 docker/tests/test_file_server.py --verbose           # 详细输出
"""

import json
import sys
import time
import urllib.request
import urllib.error
import argparse
from dataclasses import dataclass, field

# ============================================================================
# 配置
# ============================================================================

DEFAULT_BASE = "http://127.0.0.1:60001"

# ============================================================================
# HTTP 工具
# ============================================================================

@dataclass
class Response:
    status: int
    body: dict
    raw: str

    @property
    def success(self) -> bool:
        return self.body.get("success", False) or self.body.get("status") == "ok"


def request(method: str, url: str, json_body=None, files=None, timeout=30) -> Response:
    """发送 HTTP 请求, 返回 Response"""
    data = None
    headers = {}

    if json_body is not None:
        data = json.dumps(json_body).encode()
        headers["Content-Type"] = "application/json"
    elif files is not None:
        # multipart/form-data
        boundary = f"----test{int(time.time() * 1000)}"
        body_parts = []
        for key, (filename, content) in files.items():
            body_parts.append(f"--{boundary}\r\n".encode())
            body_parts.append(
                f'Content-Disposition: form-data; name="{key}"; filename="{filename}"\r\n'
                f"Content-Type: application/octet-stream\r\n\r\n".encode()
            )
            body_parts.append(content if isinstance(content, bytes) else content.encode())
            body_parts.append(b"\r\n")
        body_parts.append(f"--{boundary}--\r\n".encode())
        data = b"".join(body_parts)
        headers["Content-Type"] = f"multipart/form-data; boundary={boundary}"

    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode()
            body = json.loads(raw) if raw else {}
            return Response(resp.status, body, raw)
    except urllib.error.HTTPError as e:
        raw = e.read().decode() if e.fp else ""
        body = json.loads(raw) if raw else {}
        return Response(e.code, body, raw)
    except Exception as e:
        return Response(0, {"error": str(e)}, str(e))


# ============================================================================
# 测试框架
# ============================================================================

@dataclass
class TestResult:
    name: str
    passed: bool
    detail: str = ""


class TestRunner:
    def __init__(self, base_url: str, verbose: bool = False):
        self.base = base_url.rstrip("/")
        self.verbose = verbose
        self.results: list[TestResult] = []
        self.project_id = f"test-py-{int(time.time())}"

    def url(self, path: str) -> str:
        return f"{self.base}{path}"

    def run(self, name: str, resp: Response, expect_success: bool = True):
        """断言并记录结果"""
        passed = resp.success if expect_success else (resp.status > 0)
        detail = ""
        if not passed:
            err = resp.body.get("error", {})
            detail = err.get("message", resp.body.get("message", resp.raw[:80]))
        self.results.append(TestResult(name, passed, detail))
        icon = "✅" if passed else "❌"
        line = f"  {icon} {name}"
        if not passed and detail:
            line += f" — {detail[:60]}"
        elif self.verbose and passed:
            extra = resp.body.get("message", "")
            if extra:
                line += f" ({extra[:40]})"
        print(line)
        return resp

    # ===== 通用 HTTP 方法 =====

    def get(self, path: str) -> Response:
        return request("GET", self.url(path))

    def post_json(self, path: str, body: dict) -> Response:
        return request("POST", self.url(path), json_body=body)

    def post_file(self, path: str, files: dict) -> Response:
        return request("POST", self.url(path), files=files)

    # ===== 测试用例 =====

    def test_health(self):
        """file-server 健康检查"""
        resp = self.get("/health")
        self.run("health", resp)

    def test_create_project(self):
        """创建项目 (vue3 模板)"""
        resp = self.post_json(
            "/api/project/create-project",
            {"projectId": self.project_id, "templateType": "vue3"},
        )
        self.run("create-project (vue3)", resp)

    def test_get_project_content(self):
        """获取项目文件树"""
        resp = self.get(f"/api/project/get-project-content?projectId={self.project_id}")
        file_count = len(resp.body.get("files", []))
        passed = resp.success and file_count > 0
        self.results.append(TestResult("get-project-content", passed, f"{file_count} files"))
        print(f"  {'✅' if passed else '❌'} get-project-content ({file_count} files)")

    def test_git_init(self):
        """git init"""
        resp = self.post_json(
            "/api/git/init",
            {"workspaceType": "pageApp", "projectId": self.project_id},
        )
        self.run("git init", resp)

    def test_git_add(self):
        """git add (全部)"""
        resp = self.post_json(
            "/api/git/add",
            {"workspaceType": "pageApp", "projectId": self.project_id},
        )
        self.run("git add", resp)

    def test_git_commit(self):
        """git commit"""
        resp = self.post_json(
            "/api/git/commit",
            {"workspaceType": "pageApp", "projectId": self.project_id, "message": "test commit"},
        )
        commit = resp.body.get("commit", "")
        self.results.append(TestResult("git commit", resp.success, f"commit={commit[:8]}"))
        print(f"  {'✅' if resp.success else '❌'} git commit ({commit[:8]})")

    def test_git_status(self):
        """git status"""
        resp = self.get(
            f"/api/git/status?workspaceType=pageApp&projectId={self.project_id}"
        )
        self.run("git status", resp)

    def test_git_log(self):
        """git log"""
        resp = self.get(
            f"/api/git/log?workspaceType=pageApp&projectId={self.project_id}"
        )
        commits = len(resp.body.get("commits", []))
        passed = resp.success and commits > 0
        self.results.append(TestResult("git log", passed, f"{commits} commits"))
        print(f"  {'✅' if passed else '❌'} git log ({commits} commits)")

    def test_git_branches(self):
        """git branches"""
        resp = self.get(
            f"/api/git/branches?workspaceType=pageApp&projectId={self.project_id}"
        )
        branches = list(resp.body.get("branches", {}).keys())
        passed = resp.success and len(branches) > 0
        self.results.append(TestResult("git branches", passed, str(branches)))
        print(f"  {'✅' if passed else '❌'} git branches ({branches})")

    def test_git_diff(self):
        """git diff (worktree)"""
        resp = self.post_json(
            "/api/git/diff",
            {"workspaceType": "pageApp", "projectId": self.project_id, "source": "worktree"},
        )
        self.run("git diff", resp)

    def test_git_unstage(self):
        """git unstage"""
        resp = self.post_json(
            "/api/git/unstage",
            {"workspaceType": "pageApp", "projectId": self.project_id},
        )
        self.run("git unstage", resp)

    def test_build_port_pool(self):
        """build: 端口池状态"""
        resp = self.get("/api/build/port-pool-status")
        self.run("build port-pool-status", resp)

    def test_build_list_dev(self):
        """build: 列出 dev server"""
        resp = self.get("/api/build/list-dev")
        self.run("build list-dev", resp)

    def test_computer_get_file_list(self):
        """computer: 获取文件列表"""
        resp = self.get(
            "/api/computer/get-file-list?userId=test-py-u1&cId=test-py-c1"
        )
        self.run("computer get-file-list", resp)

    def test_delete_project(self):
        """删除项目"""
        resp = self.get(f"/api/project/delete-project?projectId={self.project_id}")
        deleted = len(resp.body.get("deletedDirectories", []))
        passed = resp.success and deleted > 0
        self.results.append(TestResult("delete-project", passed, f"{deleted} dirs"))
        print(f"  {'✅' if passed else '❌'} delete-project ({deleted} dirs)")

    # ===== 主入口 =====

    def run_all(self):
        """运行全部测试"""
        print(f"🔍 file-server API 测试 ({self.base})")
        print(f"📋 测试项目: {self.project_id}")
        print()

        print("--- 健康检查 ---")
        self.test_health()
        print()

        print("--- 项目 CRUD ---")
        self.test_create_project()
        self.test_get_project_content()
        print()

        print("--- Git 操作 ---")
        self.test_git_init()
        self.test_git_add()
        self.test_git_commit()
        self.test_git_status()
        self.test_git_log()
        self.test_git_branches()
        self.test_git_diff()
        self.test_git_unstage()
        print()

        print("--- Build ---")
        self.test_build_port_pool()
        self.test_build_list_dev()
        print()

        print("--- Computer ---")
        self.test_computer_get_file_list()
        print()

        print("--- 清理 ---")
        self.test_delete_project()
        print()

        # 汇总
        passed = sum(1 for r in self.results if r.passed)
        failed = sum(1 for r in self.results if not r.passed)
        total = len(self.results)

        print("=" * 50)
        print(f"✅ Passed: {passed}")
        print(f"❌ Failed: {failed}")
        print(f"📊 Total:  {total}")
        print("=" * 50)

        if failed > 0:
            print("\n失败详情:")
            for r in self.results:
                if not r.passed:
                    print(f"  ❌ {r.name}: {r.detail}")

        return 0 if failed == 0 else 1


# ============================================================================
# 入口
# ============================================================================

def main():
    parser = argparse.ArgumentParser(description="file-server API 测试")
    parser.add_argument("--base", default=DEFAULT_BASE, help=f"base URL (默认 {DEFAULT_BASE})")
    parser.add_argument("-v", "--verbose", action="store_true", help="详细输出")
    args = parser.parse_args()

    runner = TestRunner(args.base, verbose=args.verbose)
    sys.exit(runner.run_all())


if __name__ == "__main__":
    main()
