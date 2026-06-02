package db

import (
	"os"
	"testing"
	"time"
)

func TestDB(t *testing.T) {
	// 创建临时数据库
	tmpFile, err := os.CreateTemp("", "pangolin-*.db")
	if err != nil {
		t.Fatal(err)
	}
	dbPath := tmpFile.Name()
	tmpFile.Close()
	defer os.Remove(dbPath)

	// 初始化
	if err := Init(dbPath); err != nil {
		t.Fatalf("数据库初始化失败: %v", err)
	}
	defer Close()

	// 测试注册
	err = Register("test1.example.com", "192.168.1.100", 8080)
	if err != nil {
		t.Fatalf("注册失败: %v", err)
	}

	// 测试查询
	registered, d, err := IsRegistered("test1.example.com")
	if err != nil {
		t.Fatalf("查询失败: %v", err)
	}
	if !registered {
		t.Fatal("应该已注册")
	}
	if d.LocalIP != "192.168.1.100" {
		t.Errorf("期望 IP 192.168.1.100，实际 %s", d.LocalIP)
	}
	if d.LocalPort != 8080 {
		t.Errorf("期望端口 8080，实际 %d", d.LocalPort)
	}
	if !d.Online {
		t.Error("应该在线")
	}

	// 测试设置离线
	err = SetOffline("test1.example.com")
	if err != nil {
		t.Fatalf("设置离线失败: %v", err)
	}

	_, d, err = IsRegistered("test1.example.com")
	if err != nil {
		t.Fatalf("查询失败: %v", err)
	}
	if d.Online {
		t.Error("应该离线")
	}

	// 测试未注册的域名
	registered, _, err = IsRegistered("notexist.example.com")
	if err != nil {
		t.Fatalf("查询失败: %v", err)
	}
	if registered {
		t.Error("不应该已注册")
	}

	t.Log("✅ 数据库测试通过")
}

func TestGetAll(t *testing.T) {
	// 创建临时数据库
	tmpFile, err := os.CreateTemp("", "pangolin-*.db")
	if err != nil {
		t.Fatal(err)
	}
	dbPath := tmpFile.Name()
	tmpFile.Close()
	defer os.Remove(dbPath)

	// 初始化
	Init(dbPath)
	defer Close()

	// 注册多个域名
	Register("test1.com", "192.168.1.1", 8080)
	Register("test2.com", "192.168.1.2", 3000)
	Register("test3.com", "192.168.1.3", 9000)
	SetOffline("test2.com")

	// 获取全部
	domains, err := GetAll()
	if err != nil {
		t.Fatalf("获取全部失败: %v", err)
	}
	if len(domains) != 3 {
		t.Errorf("期望3个域名，实际 %d", len(domains))
	}

	// 获取在线
	online, err := GetOnline()
	if err != nil {
		t.Fatalf("获取在线失败: %v", err)
	}
	if len(online) != 2 {
		t.Errorf("期望2个在线，实际 %d", len(online))
	}

	t.Log("✅ 获取列表测试通过")
}

func TestDomainFields(t *testing.T) {
	// 验证字段存在
	d := &Domain{
		ID:          1,
		Domain:      "test.com",
		LocalIP:     "192.168.1.100",
		LocalPort:   8080,
		Online:      true,
		RegisteredAt: time.Now(),
		LastSeenAt:  time.Now(),
	}

	if d.Domain != "test.com" {
		t.Error("Domain字段错误")
	}
	if d.LocalIP != "192.168.1.100" {
		t.Error("LocalIP字段错误")
	}
	if d.LocalPort != 8080 {
		t.Error("LocalPort字段错误")
	}

	t.Log("✅ 字段测试通过")
}
