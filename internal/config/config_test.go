package config

import (
	"os"
	"testing"
)

func TestLoadConfig(t *testing.T) {
	// 创建临时配置文件
	content := `
server:
  port: 8080
  ws_path: /tunnel
  token: "test-token"

domains:
  app1.test.com:
    local_ip: 192.168.1.100
    local_port: 8080
    enabled: true

admin:
  username: admin
  password: "123456"

cache:
  enabled: true
  dir: ./cache

log:
  level: info
`
	tmpFile, err := os.CreateTemp("", "config-*.yaml")
	if err != nil {
		t.Fatal(err)
	}
	defer os.Remove(tmpFile.Name())

	if _, err := tmpFile.WriteString(content); err != nil {
		t.Fatal(err)
	}
	tmpFile.Close()

	// 测试加载
	cfg, err := Load(tmpFile.Name())
	if err != nil {
		t.Fatalf("加载配置失败: %v", err)
	}

	// 验证
	if cfg.Server.Port != 8080 {
		t.Errorf("期望端口 8080，实际 %d", cfg.Server.Port)
	}
	if cfg.Server.Token != "test-token" {
		t.Errorf("期望 token test-token，实际 %s", cfg.Server.Token)
	}
	if cfg.Server.WSPath != "/tunnel" {
		t.Errorf("期望 /tunnel，实际 %s", cfg.Server.WSPath)
	}

	// 验证域名配置
	domain, ok := cfg.Domains["app1.test.com"]
	if !ok {
		t.Fatal("域名配置未找到")
	}
	if domain.LocalIP != "192.168.1.100" {
		t.Errorf("期望 local_ip 192.168.1.100，实际 %s", domain.LocalIP)
	}
	if domain.LocalPort != 8080 {
		t.Errorf("期望 local_port 8080，实际 %d", domain.LocalPort)
	}

	// 验证后台配置
	if cfg.Admin.Username != "admin" {
		t.Errorf("期望 admin 用户名 admin，实际 %s", cfg.Admin.Username)
	}
	if cfg.Admin.Password != "123456" {
		t.Errorf("期望 admin 密码 123456，实际 %s", cfg.Admin.Password)
	}

	// 验证缓存配置
	if !cfg.Cache.Enabled {
		t.Error("缓存应启用")
	}
	if cfg.Cache.Dir != "./cache" {
		t.Errorf("期望缓存目录 ./cache，实际 %s", cfg.Cache.Dir)
	}

	t.Log("✅ 配置加载测试通过")
}

func TestDefaultValues(t *testing.T) {
	// 创建最小配置文件
	content := `
server:
  token: "test"
`
	tmpFile, err := os.CreateTemp("", "config-*.yaml")
	if err != nil {
		t.Fatal(err)
	}
	defer os.Remove(tmpFile.Name())

	if _, err := tmpFile.WriteString(content); err != nil {
		t.Fatal(err)
	}
	tmpFile.Close()

	cfg, err := Load(tmpFile.Name())
	if err != nil {
		t.Fatalf("加载配置失败: %v", err)
	}

	// 验证默认值
	if cfg.Server.Port != 8080 {
		t.Errorf("期望默认端口 8080，实际 %d", cfg.Server.Port)
	}
	if cfg.Server.TLSPort != 8443 {
		t.Errorf("期望默认TLS端口 8443，实际 %d", cfg.Server.TLSPort)
	}
	if cfg.Server.WSPath != "/tunnel" {
		t.Errorf("期望默认WS路径 /tunnel，实际 %s", cfg.Server.WSPath)
	}
	if cfg.Cache.Dir != "./cache" {
		t.Errorf("期望默认缓存目录 ./cache，实际 %s", cfg.Cache.Dir)
	}

	t.Log("✅ 默认值测试通过")
}
