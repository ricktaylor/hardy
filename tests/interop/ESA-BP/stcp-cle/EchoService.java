import esa.egos.bp.client.core.BpClient;
import esa.egos.bp.daemon.common.stubs.BundleMessage;
import esa.egos.bp.daemon.common.stubs.BundleProcessingControlFlag;
import esa.egos.bp.daemon.common.stubs.ListenRequest;
import esa.egos.bp.daemon.common.stubs.ListenResponse;
import esa.egos.bp.daemon.common.stubs.SendAduRequest;
import esa.egos.bp.daemon.common.stubs.BpDaemonServiceDefinitionGrpc;
import com.google.protobuf.ByteString;
import io.grpc.ManagedChannel;
import io.grpc.ManagedChannelBuilder;
import java.util.Iterator;

/**
 * Minimal echo service for ESA-BP interop testing.
 *
 * Connects to the ESA-BP daemon via gRPC, listens on a service number,
 * and echoes received payloads back to the source EID.
 *
 * Usage: java EchoService [service_number] [grpc_host] [grpc_port]
 */
public class EchoService {

    public static void main(String[] args) {
        int serviceNumber = args.length > 0 ? Integer.parseInt(args[0]) : 7;
        String host = args.length > 1 ? args[1] : "localhost";
        int port = args.length > 2 ? Integer.parseInt(args[2]) : 5672;

        System.out.println("Echo service starting on service " + serviceNumber
            + " (gRPC " + host + ":" + port + ")");

        ManagedChannel channel = ManagedChannelBuilder
            .forAddress(host, port)
            .usePlaintext()
            .build();

        BpDaemonServiceDefinitionGrpc.BpDaemonServiceDefinitionBlockingStub stub =
            BpDaemonServiceDefinitionGrpc.newBlockingStub(channel);

        // Subscribe to bundles on this service number
        Iterator<ListenResponse> stream = stub.listen(
            ListenRequest.newBuilder()
                .setServiceNumber(serviceNumber)
                .build());

        System.out.println("Listening for bundles...");

        while (stream.hasNext()) {
            ListenResponse response = stream.next();

            if (response.hasBundle()) {
                BundleMessage bundle = response.getBundle();
                String srcEid = bundle.getIdentity().getSrcEid();
                ByteString adu = bundle.getAdu();

                System.out.println("Received bundle from " + srcEid
                    + " (" + adu.size() + " bytes), echoing back");

                try {
                    stub.sendAdu(SendAduRequest.newBuilder()
                        .setServiceNumber(serviceNumber)
                        .setDstEndpointId(srcEid)
                        .setReportEndpointId("dtn:none")
                        .setLifetime(3600000)
                        .setAdu(adu)
                        .build());
                } catch (Exception e) {
                    System.err.println("Failed to echo bundle: " + e.getMessage());
                }
            } else if (response.hasAdminRecord()) {
                System.out.println("Received admin record (ignored)");
            } else if (response.hasError()) {
                System.err.println("Received error: " + response.getError());
            }
        }

        channel.shutdown();
        System.out.println("Echo service stopped");
    }
}
