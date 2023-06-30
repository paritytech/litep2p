package main

import (
	"context"
	"fmt"
	"net"
	"os"
	"time"

	"encoding/binary"
	"github.com/libp2p/go-libp2p"
	"github.com/libp2p/go-libp2p/p2p/protocol/ping"
	multiaddr "github.com/multiformats/go-multiaddr" 
	peerstore "github.com/libp2p/go-libp2p/core/peer"  
)

func main() {
	// golog.SetAllLoggers(golog.LevelDebug)

	socketFile := "/tmp/ping-test.sock"

	// Remove the socket file if it already exists
	_ = os.Remove(socketFile)

	// Create a Unix domain socket listener
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: socketFile, Net: "unix"})
	socket, err := listener.Accept()

	// start libpp2p node
    node, err := libp2p.New(
        libp2p.ListenAddrStrings("/ip4/127.0.0.1/tcp/0"),
        libp2p.Ping(false),
    )
    if err != nil {
        panic(err)
    }

    // configure our own ping protocol
    pingService := &ping.PingService { Host: node }
    node.SetStreamHandler(ping.ID, pingService.PingHandler)

    peerInfo := peerstore.AddrInfo {
        ID:    node.ID(),
        Addrs: node.Addrs(),
    }
    addrs, err := peerstore.AddrInfoToP2pAddrs(&peerInfo)
    fmt.Println("libp2p node address:", addrs[0])

	// addrString := addrs[0].String()
	// addrLen := len(addrString)

	// // Convert the length to a 2-byte vector
	// lenBytes := make([]byte, 2)
	// lenBytes[0] = byte(addrLen >> 8)   // Most significant byte
	// lenBytes[1] = byte(addrLen & 0xFF) // Least significant byte

	// _, err = socket.Write(lenBytes)
	// if err != nil {
	// 	fmt.Println("Error sending length:", err)
	// 	return
	// }

	// _, err = socket.Write([]byte(addrString))
	// if err != nil {
	// 	fmt.Println("Error sending string:", err)
	// 	return
	// }

	// read litep2p node's address
	lenBytes := make([]byte, 2)
	_, err = socket.Read(lenBytes)
	if err != nil {
		fmt.Println("Error reading length:", err)
		return
	}

	// Convert the length bytes to uint16
	strLen := binary.BigEndian.Uint16(lenBytes)

	// Read the string bytes from the socket
	strBytes := make([]byte, strLen)
	_, err = socket.Read(strBytes)
	if err != nil {
		fmt.Println("Error reading string:", err)
		return
	}

	// Convert the string bytes to a string
	str := string(strBytes)

	// monitor until the litep2p node closes the socket
	socketClosed := make(chan bool)
	go func() {
		buf := make([]byte, 1024)
		_, err := socket.Read(buf)
		if err != nil {
			socketClosed <- true
		}
	}()

    addr, err := multiaddr.NewMultiaddr(str)
    if err != nil {
        panic(err)
    }
    peer, err := peerstore.AddrInfoFromP2pAddr(addr)
    if err != nil {
        panic(err)
    }
    if err := node.Connect(context.Background(), *peer); err != nil {
        panic(err)
    }

	select {
		case <- socketClosed:
			listener.Close()
			os.Remove("/tmp/ping-test.sock");
		case <- time.After(20 * time.Second):
			fmt.Println("timeout, test failed");
	}

	listener.Close()
	os.Remove("/tmp/ping-test.sock")
}
