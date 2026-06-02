package proxy

import (
	"bytes"
	"io"
	"net/http"
	"testing"
)

func TestNewProxy(t *testing.T) {
	p := NewProxy("192.168.1.100", 8080)

	if p.LocalIP != "192.168.1.100" {
		t.Errorf("期望 LocalIP 192.168.1.100，实际 %s", p.LocalIP)
	}
	if p.LocalPort != 8080 {
		t.Errorf("期望 LocalPort 8080，实际 %d", p.LocalPort)
	}

	t.Log("✅ Proxy创建测试通过")
}

func TestBuildRequest(t *testing.T) {
	headers := map[string]string{
		"Content-Type": "application/json",
		"Authorization": "Bearer token123",
	}

	body := `{"name": "test"}`

	req, err := BuildRequest("POST", "http://localhost:8080/api", headers, body)
	if err != nil {
		t.Fatalf("构建请求失败: %v", err)
	}

	if req.Method != "POST" {
		t.Errorf("期望方法 POST，实际 %s", req.Method)
	}

	if req.Header.Get("Content-Type") != "application/json" {
		t.Errorf("Content-Type 错误")
	}

	if req.Header.Get("Authorization") != "Bearer token123" {
		t.Errorf("Authorization 错误")
	}

	t.Log("✅ HTTP请求构建测试通过")
}

func TestBuildRequestNoBody(t *testing.T) {
	req, err := BuildRequest("GET", "http://localhost:8080/api", nil, "")
	if err != nil {
		t.Fatalf("构建请求失败: %v", err)
	}

	if req.Method != "GET" {
		t.Errorf("期望方法 GET，实际 %s", req.Method)
	}

	t.Log("✅ 无body请求构建测试通过")
}

func TestIsHopByHop(t *testing.T) {
	tests := []struct {
		header   string
		expected bool
	}{
		{"Connection", true},
		{"Keep-Alive", true},
		{"Proxy-Authenticate", true},
		{"Proxy-Authorization", true},
		{"TE", true},
		{"Trailers", true},
		{"Transfer-Encoding", true},
		{"Upgrade", true},
		{"Content-Type", false},
		{"Authorization", false},
		{"Host", false},
	}

	for _, tt := range tests {
		result := isHopByHop(tt.header)
		if result != tt.expected {
			t.Errorf("isHopByHop(%s) 期望 %v，实际 %v", tt.header, tt.expected, result)
		}
	}

	t.Log("✅ Hop-by-Hop头测试通过")
}

func TestReadBody(t *testing.T) {
	body := []byte(`{"status": "ok"}`)
	resp := &http.Response{
		Body: io.NopCloser(bytes.NewReader(body)),
	}

	result, err := ReadBody(resp)
	if err != nil {
		t.Fatalf("读取body失败: %v", err)
	}

	if !bytes.Equal(result, body) {
		t.Errorf("期望 body %s，实际 %s", body, result)
	}

	t.Log("✅ 读取响应body测试通过")
}
