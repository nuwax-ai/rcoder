# Docker 容器网络隔离配置

## 概述

为了防止 Docker 容器访问宿主机内网地址，我们实现了多层安全防护机制。

## 安全措施

### 1. 容器能力限制（Container Capabilities）

在创建容器时，移除了以下网络相关的特权能力：

- `NET_RAW`: 禁止创建原始套接字，防止容器进行网络嗅探
- `NET_ADMIN`: 禁止网络管理操作，防止容器修改网络配置

```rust
cap_drop: Some(vec![
    "NET_RAW".to_string(),
    "NET_ADMIN".to_string(),
]),
privileged: Some(false),
```

### 2. 网络层隔离（Network Isolation）

创建专用的 `rcoder-agent-network` 桥接网络，配置如下：

- 启用容器间通信（ICC）：允许同一网络内的容器互相通信
- 启用 IP 伪装：允许容器访问外网
- 禁用主机直接绑定：防止容器直接访问宿主机端口

### 3. iptables 规则（推荐）

自动配置 iptables 规则，阻止容器访问以下内网地址段：

| 地址段           | 说明         |
| ---------------- | ------------ |
| `10.0.0.0/8`     | A 类私有地址 |
| `172.16.0.0/12`  | B 类私有地址 |
| `192.168.0.0/16` | C 类私有地址 |
| `169.254.0.0/16` | 链路本地地址 |
| `127.0.0.0/8`    | 本地回环地址 |

## 手动配置 iptables（可选）

如果自动配置失败，可以手动执行以下命令：

```bash
# 获取 Docker 网络子网
NETWORK_SUBNET=$(docker network inspect rcoder-agent-network -f '{{range .IPAM.Config}}{{.Subnet}}{{end}}')

# 阻止访问内网地址段
sudo iptables -I DOCKER-USER -s $NETWORK_SUBNET -d 10.0.0.0/8 -j DROP
sudo iptables -I DOCKER-USER -s $NETWORK_SUBNET -d 172.16.0.0/12 -j DROP
sudo iptables -I DOCKER-USER -s $NETWORK_SUBNET -d 192.168.0.0/16 -j DROP
sudo iptables -I DOCKER-USER -s $NETWORK_SUBNET -d 169.254.0.0/16 -j DROP
sudo iptables -I DOCKER-USER -s $NETWORK_SUBNET -d 127.0.0.0/8 -j DROP

# 查看规则
sudo iptables -L DOCKER-USER -n -v
```

## 验证隔离效果

在容器内测试网络访问：

```bash
# 进入容器
docker exec -it <container_name> sh

# 测试外网访问（应该成功）
ping -c 3 8.8.8.8
curl https://www.google.com

# 测试内网访问（应该失败）
ping -c 3 192.168.1.1
curl http://10.0.0.1
```

## 注意事项

1. **权限要求**: 配置 iptables 规则需要 root 权限
2. **规则持久化**: iptables 规则在系统重启后会丢失，需要配置持久化
3. **性能影响**: iptables 规则对网络性能影响很小，可以忽略不计
4. **容器间通信**: 同一网络内的容器仍然可以互相通信
5. **外网访问**: 容器仍然可以正常访问公网服务

## 故障排查

### 问题 1: iptables 规则配置失败

**原因**: 可能是权限不足或 iptables 未安装

**解决方案**:

```bash
# 检查 iptables 是否安装
which iptables

# 检查 Docker 守护进程是否以 root 运行
ps aux | grep dockerd

# 手动添加规则（需要 sudo）
sudo iptables -I DOCKER-USER -s 172.18.0.0/16 -d 192.168.0.0/16 -j DROP
```

### 问题 2: 容器无法访问外网

**原因**: IP 伪装未正确配置

**解决方案**:

```bash
# 检查 IP 转发是否启用
sysctl net.ipv4.ip_forward

# 启用 IP 转发
sudo sysctl -w net.ipv4.ip_forward=1

# 检查 NAT 规则
sudo iptables -t nat -L POSTROUTING -n -v
```

### 问题 3: 容器仍然可以访问内网

**原因**: iptables 规则未生效或被其他规则覆盖

**解决方案**:

```bash
# 查看 DOCKER-USER 链规则
sudo iptables -L DOCKER-USER -n -v --line-numbers

# 确保 DROP 规则在最前面
sudo iptables -I DOCKER-USER 1 -s 172.18.0.0/16 -d 192.168.0.0/16 -j DROP

# 清除所有规则重新配置
sudo iptables -F DOCKER-USER
```

## 安全建议

1. **定期审计**: 定期检查 iptables 规则是否生效
2. **日志记录**: 启用 iptables 日志记录，监控异常访问
3. **最小权限**: 只给容器必要的网络权限
4. **网络分段**: 将不同安全级别的容器放在不同的网络中
5. **监控告警**: 配置网络流量监控和异常告警

## 参考资料

- [Docker Network Security](https://docs.docker.com/network/security/)
- [iptables Tutorial](https://www.netfilter.org/documentation/HOWTO/packet-filtering-HOWTO.html)
- [Docker DOCKER-USER Chain](https://docs.docker.com/network/iptables/)
