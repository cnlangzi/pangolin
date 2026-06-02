package cache

import (
	"os"
	"strings"
	"testing"
)

func TestCacheKey(t *testing.T) {
	c := NewCache("./cache_test", false)

	tests := []struct {
		url      string
		expected int
	}{
		{"/api/test", 64},
		{"/api/users?id=1", 64},
		{"", 64},
	}

	for _, tt := range tests {
		key := c.Key(tt.url)
		if len(key) != tt.expected {
			t.Errorf("Key(%s) 长度期望 %d，实际 %d", tt.url, tt.expected, len(key))
		}
	}

	t.Log("✅ 缓存Key生成测试通过")
}

func TestCachePath(t *testing.T) {
	c := NewCache("/tmp/pangolin_cache", false)

	path := c.Path("/api/test")
	if !strings.HasPrefix(path, "/tmp/pangolin_cache/") {
		t.Errorf("缓存路径应以 /tmp/pangolin_cache/ 开头，实际: %s", path)
	}
	if !strings.HasSuffix(path, ".cache") {
		t.Errorf("缓存路径应以 .cache 结尾，实际: %s", path)
	}

	t.Log("✅ 缓存路径生成测试通过")
}

func TestCacheDirCreation(t *testing.T) {
	tmpDir := "/tmp/pangolin_cache_test_" + string(rune(os.Getpid()))
	c := NewCache(tmpDir, true)
	_ = c // 使用变量避免警告

	// 检查目录是否创建
	if _, err := os.Stat(tmpDir); os.IsNotExist(err) {
		t.Error("缓存目录应被创建")
	}

	// 清理
	os.RemoveAll(tmpDir)

	t.Log("✅ 缓存目录创建测试通过")
}

func TestCacheEnabled(t *testing.T) {
	c1 := NewCache("/tmp/cache1", true)
	if !c1.Enabled {
		t.Error("启用缓存时 Enabled 应为 true")
	}

	c2 := NewCache("/tmp/cache2", false)
	if c2.Enabled {
		t.Error("禁用缓存时 Enabled 应为 false")
	}

	t.Log("✅ 缓存启用状态测试通过")
}
