// Send through `notts serve` over its unix socket. Usage: go run serve_client.go SOCKET APP TITLE BODY
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strings"
)

func main() {
	sock := os.Args[1]
	client := &http.Client{Transport: &http.Transport{
		DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
			return (&net.Dialer{}).DialContext(ctx, "unix", sock)
		},
	}}
	payload, _ := json.Marshal(map[string]any{"app": os.Args[2], "severity": "warn", "title": os.Args[3], "body": os.Args[4]})
	resp, err := client.Post("http://notts/v1/send", "application/json", strings.NewReader(string(payload)))
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	// 200 delivered, 202 queued in the outbox, 429 slow down (see Retry-After), 502 not delivered
	fmt.Println(resp.StatusCode, string(body))
}
