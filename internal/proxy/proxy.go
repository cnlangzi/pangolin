package proxy

import (
	"bytes"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/http/httputil"
	"strings"
)

// Proxy HTTP代理
type Proxy struct {
	LocalIP   string
	LocalPort int
}

func NewProxy(localIP string, localPort int) *Proxy {
	return &Proxy{
		LocalIP:   localIP,
		LocalPort: localPort,
	}
}

// Request 转发请求到内网
func (p *Proxy) Request(req *http.Request) (*http.Response, error) {
	// 构建目标URL
	target := fmt.Sprintf("http://%s:%d%s", p.LocalIP, p.LocalPort, req.URL.Path)
	if req.URL.RawQuery != "" {
		target += "?" + req.URL.RawQuery
	}

	// 创建代理请求
	proxyReq, err := http.NewRequest(req.Method, target, req.Body)
	if err != nil {
		return nil, err
	}

	// 复制请求头
	for k, v := range req.Header {
		// 跳过Hop-by-Hop头
		if isHopByHop(k) {
			continue
		}
		proxyReq.Header[k] = v
	}

	// 修改Host头
	proxyReq.Host = fmt.Sprintf("%s:%d", p.LocalIP, p.LocalPort)

	// 发送请求
	client := &http.Client{
		Timeout: 30 * 1000000000, // 30秒
	}
	
	resp, err := client.Do(proxyReq)
	if err != nil {
		return nil, err
	}

	return resp, nil
}

// Response 转发响应到客户端
func (p *Proxy) Response(resp *http.Response, w http.ResponseWriter) error {
	// 复制响应头
	for k, v := range resp.Header {
		if isHopByHop(k) {
			continue
		}
		w.Header()[k] = v
	}

	// 设置状态码
	w.WriteHeader(resp.StatusCode)

	// 复制body
	if resp.Body != nil {
		defer resp.Body.Close()
		_, err := io.Copy(w, resp.Body)
		return err
	}

	return nil
}

// DumpRequest 打印请求详情（调试用）
func DumpRequest(req *http.Request) {
	dump, err := httputil.DumpRequest(req, true)
	if err != nil {
		log.Printf("❌ Dump失败: %s", err)
		return
	}
	log.Printf("📨 请求:\n%s", string(dump))
}

// DumpResponse 打印响应详情（调试用）
func DumpResponse(resp *http.Response) {
	dump, err := httputil.DumpResponse(resp, true)
	if err != nil {
		log.Printf("❌ Dump失败: %s", err)
		return
	}
	log.Printf("📬 响应:\n%s", truncate(string(dump), 500))
}

// isHopByHop 检查是否是Hop-by-Hop头
func isHopByHop(key string) bool {
	hopByHop := []string{
		"connection",
		"keep-alive",
		"proxy-authenticate",
		"proxy-authorization",
		"te",
		"trailers",
		"transfer-encoding",
		"upgrade",
	}
	key = strings.ToLower(key)
	for _, h := range hopByHop {
		if key == h {
			return true
		}
	}
	return false
}

// truncate 截断字符串
func truncate(s string, max int) string {
	if len(s) <= max {
		return s
	}
	return s[:max] + "..."
}

// ReadBody 读取响应body
func ReadBody(resp *http.Response) ([]byte, error) {
	if resp.Body == nil {
		return nil, nil
	}
	defer resp.Body.Close()
	return io.ReadAll(resp.Body)
}

// BuildRequest 从Message构建HTTP请求
func BuildRequest(method, urlStr string, headers map[string]string, body string) (*http.Request, error) {
	var bodyReader io.Reader
	if body != "" {
		bodyReader = bytes.NewBufferString(body)
	}

	req, err := http.NewRequest(method, urlStr, bodyReader)
	if err != nil {
		return nil, err
	}

	for k, v := range headers {
		req.Header.Set(k, v)
	}

	return req, nil
}
