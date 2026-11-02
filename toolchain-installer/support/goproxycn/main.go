package main

import (
	"fmt"
	"os"
	"os/exec"
)

const proxy = "https://goproxy.cn,direct"

func main() {
	action := "status"
	if len(os.Args) > 1 {
		action = os.Args[1]
	}

	switch action {
	case "on":
		mustRun("env", "-w", "GOPROXY="+proxy)
		mustRun("env", "-w", "GOSUMDB=sum.golang.google.cn")
		status("on")
	case "off":
		mustRun("env", "-u", "GOPROXY")
		mustRun("env", "-u", "GOSUMDB")
		status("off")
	case "status":
		status("status")
	case "help", "-h", "--help":
		fmt.Println("usage: goproxycn [on|off|status]")
	default:
		fmt.Fprintf(os.Stderr, "unknown action %q\n", action)
		os.Exit(2)
	}
}

func status(action string) {
	output, err := exec.Command("go", "env", "GOPROXY", "GOSUMDB").Output()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	if action != "off" || len(output) > 0 {
		fmt.Print(string(output))
	}
}

func mustRun(name string, args ...string) {
	command := exec.Command(name, args...)
	command.Stdout = os.Stdout
	command.Stderr = os.Stderr
	if err := command.Run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
