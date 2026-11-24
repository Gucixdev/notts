// Send one notification through the notts CLI. Usage: go run send.go APP SEVERITY TITLE [BODY]
package main

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
)

// notify returns the JSON report and the exit code (0 delivered, 1 failed, 2 usage/config, 3 queued).
func notify(app, severity, title, body string) (string, int, error) {
	cmd := exec.Command("notts", "send", "--app", app, "--severity", severity, "--title", title, "--json")
	cmd.Stdin = strings.NewReader(body)
	out, err := cmd.Output()
	var ee *exec.ExitError
	if errors.As(err, &ee) {
		return string(out), ee.ExitCode(), nil
	}
	return string(out), 0, err
}

func main() {
	body := ""
	if len(os.Args) > 4 {
		body = os.Args[4]
	}
	out, code, err := notify(os.Args[1], os.Args[2], os.Args[3], body)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	fmt.Print(out)
	os.Exit(code)
}
