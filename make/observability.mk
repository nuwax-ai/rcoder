# 可观测性日志链路（阶段一 B3：Loki + fluent-bit 复刻生产采集）
# 依赖主栈已在跑（make dev-up）；rcoder.log 由 B5 的 RCODER_LOG_TO_FILE 门禁产生

COMPOSE_FILE := docker/docker-compose.yml

## 查看日志链路帮助
logs-help:
	@echo "📋 可观测性日志链路："
	@echo "  make logs-up        启动 Loki + fluent-bit（并重启 Grafana 装载合并数据源）"
	@echo "  make logs-down      停止 Loki + fluent-bit"
	@echo "  make logs-query     查询 Loki，用法: make logs-query Q='{job=\"fluent-bit\"} |= \"关键字\"'"
	@echo "  make logs-fidelity  fluent-bit/Loki 保真度自检（health/storage/ready/series）"

## 启动本地日志链路（Loki + fluent-bit；Grafana 需重启以装载 00-observability.yml）
logs-up:
	@echo "🚀 启动 Loki + fluent-bit ..."
	docker-compose -f $(COMPOSE_FILE) up -d loki fluent-bit
	@echo "🔄 重启 Grafana 装载合并数据源 ..."
	docker-compose -f $(COMPOSE_FILE) restart grafana
	@echo "✅ 日志链路已启动（Loki http://127.0.0.1:3100，Grafana http://127.0.0.1:3000）"

## 停止本地日志链路
logs-down:
	docker-compose -f $(COMPOSE_FILE) stop loki fluent-bit

## 查询 Loki（用法: make logs-query Q='{job="fluent-bit"} |= "Server starting"'）
logs-query:
	@echo "🔎 query: $(Q)"
	@curl -sG 'http://127.0.0.1:3100/loki/api/v1/query_range' \
		--data-urlencode 'query=$(Q)' \
		--data-urlencode 'limit=10' | jq -r '.data.result[]? | .values[]?[1]'

## 日志链路保真度自检：fluent-bit health/storage、Loki ready、当前 series 标签
logs-fidelity:
	@echo "== fluent-bit health =="
	@curl -s http://127.0.0.1:2020/api/v1/health; echo
	@echo "== fluent-bit storage（chunks 落盘状态） =="
	@curl -s http://127.0.0.1:2020/api/v1/storage | jq .
	@echo "== fluent-bit 输出指标（loki 重试/丢弃） =="
	@curl -s http://127.0.0.1:2020/api/v1/metrics/prometheus | grep -E 'fluentbit_output_(retries|errors|dropped)' || echo "(无重试/错误/丢弃 = 健康)"
	@echo "== Loki ready =="
	@curl -s http://127.0.0.1:3100/ready; echo
	@echo "== Loki 当前 series 标签 =="
	@curl -s 'http://127.0.0.1:3100/loki/api/v1/labels' | jq -r '.data[]?' | head -30

.PHONY: logs-help logs-up logs-down logs-query logs-fidelity
