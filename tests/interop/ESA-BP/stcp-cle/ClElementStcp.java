package esa.egos.bp.convergence.layers.elements;

import esa.egos.bp.convergence.layers.adapter.api.ClAdapterInterface;
import esa.egos.bp.convergence.layers.adapter.api.DataFlowDirection;
import esa.egos.bp.convergence.layers.enums.ActivationState;
import esa.egos.bp.convergence.layers.enums.ConnectionState;
import esa.egos.bp.mib.api.enums.CoreElementParams;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.logging.Logger;

/**
 * STCP Convergence Layer Element for interoperability testing.
 *
 * Wire format: 4-byte big-endian length prefix followed by raw bundle bytes.
 * Compatible with Hardy's STCP framing mode.
 */
public class ClElementStcp extends ClSendReceiveElement {

  public static enum CoreStcpParams {
    destinationIP,
    destinationPort,
    listeningPort
  }

  private static final Logger logger = Logger.getLogger(ClElementStcp.class.getName());
  private static final int SOCKET_TIMEOUT = 2000;

  private static HashMap<String, Object> ecm;

  private String destAddress;
  private int destPort;
  private int listenPort;
  private ServerSocket serverSocket;
  private Thread listenerThread;
  private volatile boolean receiving = false;

  public ClElementStcp(ClAdapterInterface clai, Map<String, Object> elementInfo) {
    super(clai, elementInfo);
    @SuppressWarnings("unchecked")
    HashMap<String, Object> specific =
        (HashMap<String, Object>) elementInfo.get(CoreElementParams.elementSpecific.name());
    ecm = specific;
    this.destAddress = (String) ecm.get(CoreStcpParams.destinationIP.name());
    this.destPort = (int) ecm.get(CoreStcpParams.destinationPort.name());
    this.listenPort = (int) ecm.get(CoreStcpParams.listeningPort.name());
  }

  @Override
  public void activate() {
    if (isActive()) {
      return;
    }
    try {
      setActivationState(ActivationState.ACTIVE);
      super.activate();

      receiving = true;
      listenerThread = new Thread(new StcpListener());
      listenerThread.start();

      setConnectionState(ConnectionState.CONNECTED, true);
      logger.info("STCP element activated — listening on port " + listenPort
          + ", sending to " + destAddress + ":" + destPort);
    } catch (Exception e) {
      logger.warning("Failed to activate STCP element: " + e.getMessage());
      deactivate();
    }
  }

  @Override
  public void deactivate() {
    if (!isActive()) {
      return;
    }
    setActivationState(ActivationState.INACTIVE);
    receiving = false;
    if (serverSocket != null && !serverSocket.isClosed()) {
      try { serverSocket.close(); } catch (IOException ignored) {}
    }
    setConnectionState(ConnectionState.NOT_CONNECTED, true);
    super.deactivate();
    logger.info("STCP element deactivated");
  }

  @Override
  protected void doReceive(BundleData bp) {
    getAdapter().receiveBundle(bp);
  }

  @Override
  protected void doSend(BundleData bp) {
    List<byte[]> bundlesData = bp.getBundleData();
    for (byte[] bundle : bundlesData) {
      try (Socket sock = new Socket(destAddress, destPort);
           DataOutputStream out = new DataOutputStream(sock.getOutputStream())) {
        out.writeInt(bundle.length);
        out.write(bundle);
        out.flush();
        logger.fine("STCP: sent " + bundle.length + " bytes to " + destAddress + ":" + destPort);
      } catch (IOException e) {
        logger.warning("STCP send failed: " + e.getMessage());
        if (!bp.getIdList().isEmpty()) {
          this.getAdapter().bundleSendFailedProcedure(bp);
        }
        return;
      }
    }
    this.getAdapter().nextStackLayer(bp, DataFlowDirection.SEND);
  }

  @SuppressWarnings("unchecked")
  public static void validateElementConfig(HashMap<String, Object> configMap)
      throws esa.egos.bp.convergence.layers.elements.exceptions.ConvergenceLayerElementException {
    ClElement.validateElementConfig(configMap);
  }

  private class StcpListener implements Runnable {
    @Override
    public void run() {
      try {
        serverSocket = new ServerSocket(listenPort);
        serverSocket.setSoTimeout(SOCKET_TIMEOUT);
        logger.info("STCP listener started on port " + listenPort);

        while (receiving) {
          Socket client;
          try {
            client = serverSocket.accept();
          } catch (SocketTimeoutException e) {
            continue;
          }

          try (DataInputStream in = new DataInputStream(client.getInputStream())) {
            while (true) {
              int length = in.readInt();
              if (length <= 0 || length > 65536) {
                break;
              }
              byte[] bundle = new byte[length];
              in.readFully(bundle);
              logger.fine("STCP: received " + length + " bytes");
              doReceive(new BundleData(bundle, 1));
            }
          } catch (java.io.EOFException e) {
            // Connection closed normally
          } catch (IOException e) {
            logger.fine("STCP connection error: " + e.getMessage());
          } finally {
            try { client.close(); } catch (IOException ignored) {}
          }
        }
      } catch (IOException e) {
        if (receiving) {
          logger.severe("STCP listener failed: " + e.getMessage());
        }
      }
    }
  }
}
