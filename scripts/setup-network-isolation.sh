#!/bin/bash

# 在宿主机上配置 Docker 容器网络隔离规则
# 用法: sudo ./scripts/setup-network-isolation.sh

set -e

echo "🔒 配置 Docker 容器网络隔离规则"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

# 检查是否有 root 权限
if [ "$EUID" -ne 0 ]; then 
    echo "❌ 错误: 此脚本需要 root 权限"
    echo "请使用: sudo $0"
    exit 1
fi

# 检查 iptables 是否安装
if ! command -v iptables &> /dev/null; then
    echo "❌ 错误: iptables 未安装"
    echo "请先安装 iptables"
    exit 1
fi

# 内网地址段列表
PRIVATE_NETWORKS=(
    "10.0.0.0/8"       # A类私有地址
    "172.16.0.0/12"    # B类私有地址
    "192.168.0.0/16"   # C类私有地址
    "169.254.0.0/16"   # 链路本地地址
    "127.0.0.0/8"      # 本地回环地址
)

# 查找所有 rcoder- 开头的网络
echo "🔍 查找 rcoder- 开头的 Docker 网络..."
NETWORKS=$(docker network ls --format '{{.Name}}' | grep '^rcoder-' || true)

if [ -z "$NETWORKS" ]; then
    echo "⚠️  未找到 rcoder- 开头的网络"
    echo "💡 网络会在创建容器时自动创建"
    echo ""
    echo "如果需要为特定网络配置规则，请手动指定网络名称："
    echo "  sudo $0 <network_name>"
    exit 0
fi

echo "找到以下网络:"
echo "$NETWORKS" | sed 's/^/  - /'
echo ""

# 为每个网络配置规则
for NETWORK_NAME in $NETWORKS; do
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "📡 配置网络: $NETWORK_NAME"
    echo ""
    
    # 获取网络子网
    SUBNET=$(docker network inspect "$NETWORK_NAME" -f '{{range .IPAM.Config}}{{.Subnet}}{{end}}' 2>/dev/null || echo "")
    
    if [ -z "$SUBNET" ]; then
        echo "⚠️  无法获取网络 $NETWORK_NAME 的子网信息，跳过"
        continue
    fi
    
    echo "🌐 子网: $SUBNET"
    echo ""
    
    # 检查规则是否已存在
    EXISTING_RULES=$(iptables -L DOCKER-USER -n -v 2>/dev/null | grep "$SUBNET" || true)
    
    if [ -n "$EXISTING_RULES" ]; then
        echo "✅ 规则已存在，跳过"
        echo "$EXISTING_RULES" | sed 's/^/  /'
        echo ""
        continue
    fi
    
    # 为每个内网地址段添加规则
    ADDED_COUNT=0
    for PRIVATE_NET in "${PRIVATE_NETWORKS[@]}"; do
        RULE="iptables -I DOCKER-USER -s $SUBNET -d $PRIVATE_NET -j DROP"
        
        if $RULE 2>/dev/null; then
            echo "✅ 已阻止访问: $PRIVATE_NET"
            ((ADDED_COUNT++))
        else
            echo "⚠️  添加规则失败: $PRIVATE_NET"
        fi
    done
    
    echo ""
    echo "✅ 网络 $NETWORK_NAME 配置完成: 新增 $ADDED_COUNT 条规则"
    echo ""
done

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "📊 当前 DOCKER-USER 链规则:"
echo ""
iptables -L DOCKER-USER -n -v --line-numbers
echo ""

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "✅ 配置完成！"
echo ""
echo "💡 提示:"
echo "  - 规则在系统重启后会丢失，需要重新配置"
echo "  - 可以将此脚本添加到系统启动脚本中"
echo "  - 查看规则: sudo iptables -L DOCKER-USER -n -v"
echo "  - 删除规则: sudo iptables -D DOCKER-USER <规则编号>"
echo ""
