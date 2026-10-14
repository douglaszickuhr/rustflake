FROM alpine:3.20
RUN apk add --no-cache openssh-server \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh rb \
 && echo 'rb:*' | chpasswd -e \
 && mkdir -p /home/rb/.ssh \
 && sed -i '/^AllowTcpForwarding/d;/^PasswordAuthentication/d;/^StrictModes/d' /etc/ssh/sshd_config \
 && printf 'AllowTcpForwarding yes\nPasswordAuthentication no\nStrictModes no\n' >> /etc/ssh/sshd_config
CMD ["/usr/sbin/sshd", "-D", "-e"]
