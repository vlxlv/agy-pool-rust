// Command goside drives the Go storage package against a shared state DB for rust/tests/interop.rs.
package main

import (
	"bufio"
	"fmt"
	"os"

	"github.com/vlxlv/agy-pool-go/internal/config"
	"github.com/vlxlv/agy-pool-go/internal/storage"
)

func main() {
	config.SetTestMode(true)
	if err := config.ConfigureDataDir(os.Args[1]); err != nil {
		fmt.Println("ERR", err)
		os.Exit(1)
	}
	switch os.Args[2] {
	case "seed": // create the DB and two accounts
		err := storage.PoolTransaction(func(p *storage.Pool) error {
			p.Accounts = append(p.Accounts,
				&storage.Account{ID: "acc_1", Name: "Main"},
				&storage.Account{ID: "acc_2", Name: "Work"})
			return nil
		})
		fmt.Println("OK", err)
	case "hold": // hold a lease on acc_1 until stdin closes
		release, err := storage.LeaseInFlight("acc_1")
		if err != nil {
			fmt.Println("ERR", err)
			os.Exit(1)
		}
		fmt.Println("HELD")
		bufio.NewReader(os.Stdin).ReadString('\n')
		release()
	case "remote": // print what Go sees as other processes' in-flight
		_, release, err := storage.ReserveInFlight(func(remote map[string]int64) string {
			fmt.Printf("REMOTE acc_1=%d\n", remote["acc_1"])
			return "acc_2"
		})
		if err == nil {
			release()
		}
	case "names": // print account names Go loads
		p, err := storage.LoadPool()
		if err != nil {
			fmt.Println("ERR", err)
			os.Exit(1)
		}
		for _, a := range p.Accounts {
			fmt.Println("NAME", a.ID, a.Name)
		}
	}
}
